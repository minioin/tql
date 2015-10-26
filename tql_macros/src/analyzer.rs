//! Semantic analyzer.

use std::collections::{HashMap, HashSet};

use syntax::ast::{BinOp_, Expr, Path, SpannedIdent};
use syntax::ast::Expr_::{ExprAssign, ExprBinary, ExprCall, ExprCast, ExprLit, ExprMethodCall, ExprParen, ExprPath, ExprRange, ExprUnary};
use syntax::ast::FloatTy;
use syntax::ast::IntTy;
use syntax::ast::Lit_::{LitBool, LitByte, LitByteStr, LitChar, LitFloat, LitFloatUnsuffixed, LitInt, LitStr};
use syntax::ast::LitIntType::{SignedIntLit, UnsignedIntLit, UnsuffixedIntLit};
use syntax::ast::UintTy;
use syntax::ast::UnOp::{UnNeg, UnNot};
use syntax::codemap::{Span, Spanned};
use syntax::ptr::P;

use ast::{self, Assignment, Expression, Filter, FilterExpression, Filters, Identifier, Join, Limit, LogicalOperator, Order, RelationalOperator, RValue, Query, TypedField};
use error::{Error, SqlResult, res};
use gen::ToSql;
use parser::{MethodCall, MethodCalls};
use plugin::number_literal;
use state::{SqlFields, SqlTables, get_primary_key_field, methods_singleton, singleton};
use string::find_near;
use types::Type;

/// The type of the SQL query.
enum SqlQueryType {
    CreateTable,
    Delete,
    Drop,
    Insert,
    Select,
    Update,
}

/// The query data gathered during the analyze.
type QueryData = (FilterExpression, Vec<Join>, Limit, Vec<Order>, Vec<Assignment>, Vec<TypedField>, SqlQueryType);

/// Analyze and transform the AST.
pub fn analyze(method_calls: MethodCalls, sql_tables: &SqlTables) -> SqlResult<Query> {
    // TODO: vérifier que la suite d’appels de méthode est valide (de même que l’ordre pour filter).
    let mut errors = vec![];

    let table_name = method_calls.name.clone();
    if !sql_tables.contains_key(&table_name) {
        unknown_table_error(&table_name, method_calls.position, sql_tables, &mut errors);
    }

    check_methods(&method_calls, &mut errors);

    let table = sql_tables.get(&table_name);
    let calls = &method_calls.calls;

    let (fields, filter_expression, joins, limit, order, assignments, typed_fields, query_type) =
        match table {
            Some(table) => {
                let (filter_expression, joins, limit, order, assignments, typed_fields, query_type) = try!(process_methods(&calls, table, &table_name));
                let fields = get_query_fields(table, &table_name, &joins, sql_tables);
                (fields, filter_expression, joins, limit, order, assignments, typed_fields, query_type)

            },
            None => (vec![], FilterExpression::NoFilters, vec![], Limit::NoLimit, vec![], vec![], vec![], SqlQueryType::Select),
        };

    res(new_query(fields, filter_expression, joins, limit, order, assignments, typed_fields, query_type, table_name), errors)
}

/// Analyze the types of the `Assignment`s.
fn analyze_assignments_types(assignments: &[Assignment], table_name: &str, errors: &mut Vec<Error>) {
    for assignment in assignments {
        check_field_type(table_name, &RValue::Identifier(assignment.identifier.clone()), &assignment.value, errors);
    }
}

/// Analyze the types of the `FilterExpression`.
fn analyze_filter_types(filter: &FilterExpression, table_name: &str, errors: &mut Vec<Error>) {
    // TODO: vérifier que les opérateurs sont utilisé avec les bons types.
    match *filter {
        FilterExpression::Filter(ref filter) => {
            check_field_type(table_name, &filter.operand1, &filter.operand2, errors);
        },
        FilterExpression::Filters(ref filters) => {
            analyze_filter_types(&*filters.operand1, table_name, errors);
            analyze_filter_types(&*filters.operand2, table_name, errors);
        },
        FilterExpression::NegFilter(ref filter) => {
            analyze_filter_types(filter, table_name, errors);
        },
        FilterExpression::NoFilters => (),
        FilterExpression::ParenFilter(ref filter) => {
            analyze_filter_types(filter, table_name, errors);
        }
    }
}

/// Analyze the types of the `Limit`.
fn analyze_limit_types(limit: &Limit, errors: &mut Vec<Error>) {
    match *limit {
        Limit::EndRange(ref expression) => check_type(&Type::I64, expression, errors),
        Limit::Index(ref expression) => check_type(&Type::I64, expression, errors),
        Limit::LimitOffset(ref expression1, ref expression2) => {
            check_type(&Type::I64, expression1, errors);
            check_type(&Type::I64, expression2, errors);
        },
        Limit::NoLimit => (),
        Limit::Range(ref expression1, ref expression2) => {
            check_type(&Type::I64, expression1, errors);
            check_type(&Type::I64, expression2, errors);
        },
        Limit::StartRange(ref expression) => check_type(&Type::I64, expression, errors),
    }
}

/// Analyze the literal types in the `Query`.
pub fn analyze_types(query: Query) -> SqlResult<Query> {
    let mut errors = vec![];
    match query {
        Query::CreateTable { .. } => (), // Nothing to analyze.
        Query::Delete { ref filter, ref table } => {
            analyze_filter_types(filter, &table, &mut errors);
        },
        Query::Drop { .. } => (), // Nothing to analyze.
        Query::Insert { ref assignments, ref table } => {
            analyze_assignments_types(assignments, &table, &mut errors);
        },
        Query::Select { ref filter, ref limit, ref table, .. } => {
            analyze_filter_types(filter, &table, &mut errors);
            analyze_limit_types(limit, &mut errors);
        },
        Query::Update { ref assignments, ref filter, ref table } => {
            analyze_filter_types(filter, &table, &mut errors);
            analyze_assignments_types(assignments, &table, &mut errors);
        },
    }
    res(query, errors)
}

/// Convert an `Expression` to an `Assignment`.
fn argument_to_assignment(arg: &Expression, table_name: &str, table: &SqlFields) -> SqlResult<Assignment> {
    let mut errors = vec![];
    let mut assignment = Assignment {
        identifier: "".to_owned(),
        value: number_literal(0),
    };
    if let ExprAssign(ref expr1, ref expr2) = arg.node {
        assignment.value = expr2.clone();
        if let ExprPath(_, ref path) = expr1.node {
            assignment.identifier = path.segments[0].identifier.to_string();
            check_field(&assignment.identifier, path.span, table_name, table, &mut errors);
        }
        else {
            errors.push(Error::new(
                "Expected identifier".to_owned(),
                arg.span,
            ));
        }
    }
    else {
        errors.push(Error::new(
            "Expected assignment".to_owned(),
            arg.span,
        ));
    }
    res(assignment, errors)
}

/// Convert an `Expression` to a `Join`
fn argument_to_join(arg: &Expression, table_name: &str, table: &SqlFields) -> SqlResult<Join> {
    let mut errors = vec![];
    let mut join = Join {
        left_field: "".to_owned(),
        left_table: "".to_owned(),
        right_field: "".to_owned(),
        right_table: "".to_owned(),
    };

    match arg.node {
        ExprPath(None, ref path) => {
            let identifier = path.segments[0].identifier.to_string();
            check_field(&identifier, path.span, table_name, table, &mut errors);
            match table.get(&identifier) {
                Some(&Spanned { node: ref field_type, .. }) => {
                    if let &Type::Custom(ref related_table_name) = field_type {
                        let tables = singleton();
                        match tables.get(related_table_name).and_then(|table| get_primary_key_field(table)) {
                            Some(primary_key_field) =>
                                join = Join {
                                    left_field: identifier,
                                    left_table: table_name.to_owned(),
                                    right_field: primary_key_field,
                                    right_table: related_table_name.clone(),
                                },
                            None => errors.push(no_primary_key(related_table_name, arg.span)), // TODO: utiliser la vraie position.
                        }
                    }
                    else {
                        errors.push(Error::new_with_code(
                            format!("mismatched types:\n expected `ForeignKey<_>`,\n    found `{}`", field_type),
                            arg.span,
                            "E0308",
                        ));
                    }
                },
                None => (), // This case is handled the check_field() call above.
            }
        }
        _ => {
            errors.push(Error::new(
                "Expected identifier".to_owned(),
                arg.span,
            ));
        }
    }
    res(join, errors)
}

/// Convert an `Expression` to an `Order`.
fn argument_to_order(arg: &Expression, table_name: &str, table: &SqlFields) -> SqlResult<Order> {
    fn identifier(arg: &Expression, identifier: &Expr, table_name: &str, table: &SqlFields) -> SqlResult<String> {
        let mut errors = vec![];
        if let ExprPath(_, Path { ref segments, span, .. }) = identifier.node {
            if segments.len() == 1 {
                let identifier = segments[0].identifier.to_string();
                check_field(&identifier, span, table_name, table, &mut errors);
                return res(identifier, errors);
            }
        }
        Err(vec![Error::new(
            "Expected an identifier".to_owned(),
            arg.span,
        )])
    }

    let mut errors = vec![];
    let order =
        match arg.node {
            ExprUnary(UnNeg, ref expr) => {
                let ident = try!(identifier(&arg, expr, table_name, table));
                Order::Descending(ident)
            }
            ExprPath(None, ref path) => {
                let identifier = path.segments[0].identifier.to_string();
                check_field(&identifier, path.span, table_name, table, &mut errors);
                Order::Ascending(identifier)
            }
            _ => {
                errors.push(Error::new(
                    "Expected - or identifier".to_owned(),
                    arg.span,
                ));
                Order::Ascending("".to_owned())
            }
        };
    res(order, errors)
}

/// Convert a slice of `Expression` to a `Limit`.
fn arguments_to_limit(expression: &P<Expr>) -> SqlResult<Limit> {
    let mut errors = vec![];
    let limit =
        match expression.node {
            ExprRange(None, Some(ref range_end)) => {
                Limit::EndRange(range_end.clone())
            }
            ExprRange(Some(ref range_start), None) => {
                Limit::StartRange(range_start.clone())
            }
            ExprRange(Some(ref range_start), Some(ref range_end)) => {
                // TODO: vérifier que range_start < range_end.
                Limit::Range(range_start.clone(), range_end.clone())
            }
            ExprLit(_) | ExprPath(_, _) | ExprCall(_, _) | ExprMethodCall(_, _, _) | ExprBinary(_, _, _) | ExprUnary(_, _) | ExprCast(_, _)  => {
                Limit::Index(expression.clone())
            }
            _ => {
                errors.push(Error::new(
                    "Expected index range or number expression".to_owned(),
                    expression.span,
                ));
                Limit::NoLimit
            }
        };

    // TODO: vérifier si la limite ou le décalage est 0. Le cas échéant, ne pas les mettre dans
    // la requête (optimisation).

    res(limit, errors)
}

/// Convert a `BinOp_` to an SQL `LogicalOperator`.
fn binop_to_logical_operator(binop: BinOp_) -> LogicalOperator {
    match binop {
        BinOp_::BiAdd => unreachable!(),
        BinOp_::BiSub => unreachable!(),
        BinOp_::BiMul => unreachable!(),
        BinOp_::BiDiv => unreachable!(),
        BinOp_::BiRem => unreachable!(),
        BinOp_::BiAnd => LogicalOperator::And,
        BinOp_::BiOr => LogicalOperator::Or,
        BinOp_::BiBitXor => unreachable!(),
        BinOp_::BiBitAnd => unreachable!(),
        BinOp_::BiBitOr => unreachable!(),
        BinOp_::BiShl => unreachable!(),
        BinOp_::BiShr => unreachable!(),
        BinOp_::BiEq => unreachable!(),
        BinOp_::BiLt => unreachable!(),
        BinOp_::BiLe => unreachable!(),
        BinOp_::BiNe => unreachable!(),
        BinOp_::BiGe => unreachable!(),
        BinOp_::BiGt => unreachable!(),
    }
}

/// Convert a `BinOp_` to an SQL `RelationalOperator`.
fn binop_to_relational_operator(binop: BinOp_) -> RelationalOperator {
    match binop {
        BinOp_::BiAdd => unreachable!(),
        BinOp_::BiSub => unreachable!(),
        BinOp_::BiMul => unreachable!(),
        BinOp_::BiDiv => unreachable!(),
        BinOp_::BiRem => unreachable!(),
        BinOp_::BiAnd => unreachable!(),
        BinOp_::BiOr => unreachable!(),
        BinOp_::BiBitXor => unreachable!(),
        BinOp_::BiBitAnd => unreachable!(),
        BinOp_::BiBitOr => unreachable!(),
        BinOp_::BiShl => unreachable!(),
        BinOp_::BiShr => unreachable!(),
        BinOp_::BiEq => RelationalOperator::Equal,
        BinOp_::BiLt => RelationalOperator::LesserThan,
        BinOp_::BiLe => RelationalOperator::LesserThanEqual,
        BinOp_::BiNe => RelationalOperator::NotEqual,
        BinOp_::BiGe => RelationalOperator::GreaterThan,
        BinOp_::BiGt => RelationalOperator::GreaterThanEqual,
    }
}

/// Check that the method call contains all the fields from the `table`.
fn check_insert_arguments(assignments: &[Assignment], position: Span, table: &SqlFields, errors: &mut Vec<Error>) {
    let mut names = HashSet::new();
    let mut missing_fields: Vec<&str> = vec![];
    for assignment in assignments {
        names.insert(assignment.identifier.clone());
    }
    let primary_key = get_primary_key_field(table);

    for field in table.keys() {
        if !names.contains(field) && Some(field) != primary_key.as_ref() {
            missing_fields.push(&field);
        }
    }

    if !missing_fields.is_empty() {
        let fields = "`".to_owned() + &missing_fields.join("`, `") + "`";
        errors.push(Error::new_with_code(format!("missing fields: {}", fields), position, "E0063"));
    }

    // TODO: vérifier que la clé primaire n’est pas dans les champs insérés?
}

/// Check if the `identifier` is a field in the struct `table_name`.
fn check_field(identifier: &str, position: Span, table_name: &str, table: &SqlFields, errors: &mut Vec<Error>) {
    if !table.contains_key(identifier) {
        errors.push(Error::new(
            format!("attempted access of field `{}` on type `{}`, but no field with that name was found", identifier, table_name),
            position
        ));
        let field_names = table.keys();
        if let Some(name) = find_near(identifier, field_names) {
            errors.push(Error::new_help(
                format!("did you mean {}?", name),
                position
            ));
        }
    }
}

/// Check if the type of `identifier` matches the type of the `value` expression.
fn check_field_type(table_name: &str, rvalue: &RValue, value: &Expression, errors: &mut Vec<Error>) {
    match get_field_type(table_name, rvalue) {
        Some(ref field_type) => check_type(&field_type.node, value, errors),
        None => (), // Nothing to do since this check is done in the conversion function.
    }
}

/// Check if the method `calls` exist.
fn check_methods(method_calls: &MethodCalls, errors: &mut Vec<Error>) {
    let methods = vec![
        "all".to_owned(),
        "create".to_owned(),
        "delete".to_owned(),
        "drop".to_owned(),
        "filter".to_owned(),
        "get".to_owned(),
        "insert".to_owned(),
        "join".to_owned(),
        "limit".to_owned(),
        "sort".to_owned(),
        "update".to_owned(),
    ];
    for method_call in &method_calls.calls {
        if !methods.contains(&method_call.name) {
            errors.push(Error::new(
                format!("no method named `{}` found in tql", method_call.name),
                method_call.position,
            ));
            if let Some(name) = find_near(&method_call.name, methods.iter()) {
                errors.push(Error::new_help(
                    format!("did you mean {}?", name),
                    method_call.position,
                ));
            }
        }
    }

    if method_calls.calls.is_empty() {
        let table_name = &method_calls.name;
        errors.push(Error::new_with_code(format!("`{}` is the name of a struct, but this expression uses it like a method name", table_name), method_calls.position, "E0423"));
        errors.push(Error::new_help(
            format!("did you mean to write `{}.method()`?", table_name),
            method_calls.position,
        ));
    }
}

/// Check that the specified method call did not received any arguments.
fn check_no_arguments(method_call: &MethodCall, errors: &mut Vec<Error>) {
    if !method_call.arguments.is_empty() {
        let length = method_call.arguments.len();
        let plural_verb =
            if length == 1 {
                " was"
            }
            else {
                "s were"
            };
        errors.push(Error::new_with_code(format!("this method takes 0 parameters but {} parameter{} supplied", length, plural_verb), method_call.position, "E0061"));
    }
}

/// Check if the `field_type` is compitable with the `expression`'s type.
fn check_type(field_type: &Type, expression: &Expression, errors: &mut Vec<Error>) {
    if field_type != expression {
        let literal_type = get_type(expression);
        errors.push(Error::new_with_code(
            format!("mismatched types:\n expected `{}`,\n    found `{}`", field_type, literal_type),
            expression.span,
            "E0308",
        ));
        errors.push(Error::new_note(
            "in this expansion of sql! (defined in tql)".to_owned(),
            expression.span, // TODO: mettre la position de l’appel de macro sql!.
        ));
    }
}

/// Convert the `arguments` to the `Type`.
fn convert_arguments<F, Type>(arguments: &[P<Expr>], table_name: &str, table: &SqlFields, convert_argument: F) -> SqlResult<Vec<Type>>
        where F: Fn(&Expression, &str, &SqlFields) -> SqlResult<Type> {
    let mut items = vec![];
    let mut errors = vec![];

    for arg in arguments {
        try(convert_argument(arg, table_name, table), &mut errors, |item| {
            items.push(item);
        });
    }

    res(items, errors)
}

/// Convert a Rust expression to a `FilterExpression`.
// TODO: séparer en plusieurs fonctions.
fn expression_to_filter_expression(arg: &P<Expr>, table_name: &str, table: &SqlFields) -> SqlResult<FilterExpression> {
    let mut errors = vec![];

    let dummy = FilterExpression::NoFilters;
    let filter =
        match arg.node {
            ExprBinary(Spanned { node: op, .. }, ref expr1, ref expr2) => {
                match expr1.node {
                    ExprPath(None, ref path) => {
                        let identifier = path.segments[0].identifier.to_string();
                        check_field(&identifier, path.span, table_name, table, &mut errors);
                        FilterExpression::Filter(Filter {
                            operand1: RValue::Identifier(identifier),
                            operator: binop_to_relational_operator(op), // TODO: vérifier ce qui se passe lorsqu’un opérateur logique est utilisé à la place d’un opérateur relationnel.
                            operand2: expr2.clone(),
                        })
                    },
                    ExprMethodCall(identifier, _, ref exprs) => {
                        method_call_expression_to_filter_expression(identifier, &exprs, table_name, table, op, expr2, &mut errors)
                    },
                    ExprBinary(_, _, _) | ExprParen(_) | ExprUnary(UnNot, _) => {
                        // TODO: accumuler les erreurs au lieu d’arrêter à la première.
                        let filter1 = try!(expression_to_filter_expression(expr1, table_name, table));
                        let filter2 = try!(expression_to_filter_expression(expr2, table_name, table));
                        FilterExpression::Filters(Filters {
                            operand1: Box::new(filter1),
                            operator: binop_to_logical_operator(op),
                            operand2: Box::new(filter2),
                        })
                    },
                    _ => {
                        errors.push(Error::new(
                            "Expected identifier or binary operation".to_owned(),
                            expr1.span,
                        ));
                        dummy
                    },
                }
            },
            ExprParen(ref expr) => {
                let filter = try!(expression_to_filter_expression(expr, table_name, table));
                FilterExpression::ParenFilter(box filter)
            },
            ExprUnary(UnNot, ref expr) => {
                let filter = try!(expression_to_filter_expression(expr, table_name, table));
                FilterExpression::NegFilter(box filter)
            },
            _ => {
                errors.push(Error::new(
                    "Expected binary operation".to_owned(), // TODO: corriger ce message.
                    arg.span,
                ));
                dummy
            },
        };

    res(filter, errors)
}

/// Convert an expression from a `get()` method to a FilterExpression and a Limit.
fn get_expression_to_filter_expression(arg: &P<Expr>, table_name: &str, table: &SqlFields) -> SqlResult<(FilterExpression, Limit)> {
    let primary_key_field = get_primary_key_field(table);
    match primary_key_field {
        Some(primary_key_field) =>
            match arg.node {
                ExprLit(_) | ExprPath(_, _) => {
                    let filter = FilterExpression::Filter(Filter {
                        operand1: RValue::Identifier(primary_key_field),
                        operator: RelationalOperator::Equal,
                        operand2: arg.clone(),
                    });
                    res((filter, Limit::NoLimit), vec![])
                },
                _ => expression_to_filter_expression(arg, table_name, table)
                        .and_then(|filter| Ok((filter, Limit::Index(number_literal(0))))),
            },
        None => Err(vec![no_primary_key(table_name, arg.span)]), // TODO: utiliser la vraie position.
    }
}

/// Get the type of the field if it exists.
fn get_field_type<'a>(table_name: &str, rvalue: &'a RValue) -> Option<&'a Spanned<Type>> {
    match *rvalue {
        RValue::Identifier(ref identifier) => {
            let tables = singleton();
            tables
                .get(table_name)
                .and_then(|table| table.get(identifier))
        },
        RValue::MethodCall(_) => {
            None // TODO
        },
    }
}

/// Get the query field fully qualified names.
fn get_query_fields(table: &SqlFields, table_name: &str, joins: &[Join], sql_tables: &SqlTables) -> Vec<Identifier> {
    let mut fields = vec![];
    for (field, typ) in table {
        match typ.node {
            // TODO: faire attention aux conflits de nom.
            Type::Custom(ref foreign_table) => {
                let table_name = foreign_table;
                if let Some(foreign_table) = sql_tables.get(foreign_table) {
                    if has_joins(&joins, field) {
                        for (field, typ) in foreign_table {
                            match typ.node {
                                Type::Custom(_) | Type::UnsupportedType(_) => (), // Do not add foreign key recursively.
                                _ => {
                                    fields.push(table_name.clone() + "." + &field);
                                },
                            }
                        }
                    }
                }
            },
            Type::UnsupportedType(_) => (),
            _ => {
                fields.push(table_name.to_owned() + "." + &field);
            },
        }
    }
    fields
}

/// Get the string representation of an literal `Expression` type.
fn get_type(expression: &Expression) -> &str {
    match expression.node {
        ExprLit(ref literal) => {
            match literal.node {
                LitBool(_) => "bool",
                LitByte(_) => "u8",
                LitByteStr(_) => "Vec<u8>",
                LitChar(_) => "char",
                LitFloat(_, FloatTy::TyF32) => "f32",
                LitFloat(_, FloatTy::TyF64) => "f64",
                LitFloatUnsuffixed(_) => "floating-point variable",
                LitInt(_, int_type) =>
                    match int_type {
                        SignedIntLit(IntTy::TyIs, _) => "isize",
                        SignedIntLit(IntTy::TyI8, _) => "i8",
                        SignedIntLit(IntTy::TyI16, _) => "i16",
                        SignedIntLit(IntTy::TyI32, _) => "i32",
                        SignedIntLit(IntTy::TyI64, _) => "i64",
                        UnsignedIntLit(UintTy::TyUs) => "usize",
                        UnsignedIntLit(UintTy::TyU8) => "u8",
                        UnsignedIntLit(UintTy::TyU16) => "u16",
                        UnsignedIntLit(UintTy::TyU32) => "u32",
                        UnsignedIntLit(UintTy::TyU64) => "u64",
                        UnsuffixedIntLit(_) => "integral variable",
                    }
                ,
                LitStr(_, _) => "String",
            }
        }
        _ => "",
    }
}

/// Check if there is a join in `joins` on a field named `name`.
pub fn has_joins(joins: &[Join], name: &str) -> bool {
    joins.iter()
        .map(|join| &join.left_field)
        .any(|field_name| field_name == name)
}

/// Convert a method call expression to a filter expression.
fn method_call_expression_to_filter_expression(identifier: SpannedIdent, exprs: &[Expression], table_name: &str, table: &SqlFields, op: BinOp_, expr2: &Expression, errors: &mut Vec<Error>) -> FilterExpression {
    let dummy = FilterExpression::NoFilters;
    if let ExprPath(_, ref path) = exprs[0].node {
        let object = path.segments[0].identifier.name.to_string();
        let method_name = identifier.node.name.to_string();
        let methods = methods_singleton();
        match table.get(&object) {
            Some(object_type) => {
                match methods.get(&object_type.node) {
                    Some(type_methods) => {
                        match type_methods.get(&method_name) {
                            Some(template) =>
                                FilterExpression::Filter(Filter {
                                    operand1: RValue::MethodCall(ast::MethodCall {
                                        arguments: vec![],
                                        identifier: method_name,
                                        name: object,
                                        template: template.clone(),
                                    }),
                                    operator: binop_to_relational_operator(op), // TODO: vérifier ce qui se passe lorsqu’un opérateur logique est utilisé à la place d’un opérateur relationnel.
                                    operand2: expr2.clone(),
                                }),
                                None => {
                                    unknown_method(identifier.span, &object_type.node, method_name, Some(type_methods), errors);
                                    dummy
                                }
                        }
                    },
                    None => {
                        unknown_method(identifier.span, &object_type.node, method_name, None, errors);
                        dummy
                    },
                }
            },
            None => {
                check_field(&object, identifier.span, table_name, table, errors);
                dummy
            }
        }
    }
    else {
        dummy
    }
}

/// Create a new query from all the data gathered by the method calls.
fn new_query(fields: Vec<Identifier>, filter_expression: FilterExpression, joins: Vec<Join>, limit: Limit, order: Vec<Order>, assignments: Vec<Assignment>, typed_fields: Vec<TypedField>, query_type: SqlQueryType, table_name: String) -> Query {
    match query_type {
        SqlQueryType::CreateTable =>
            Query::CreateTable {
                fields: typed_fields,
                table: table_name,
            },
        SqlQueryType::Delete =>
            Query::Delete {
                filter: filter_expression,
                table: table_name,
            },
        SqlQueryType::Drop =>
            Query::Drop {
                table: table_name,
            },
        SqlQueryType::Insert =>
            Query::Insert {
                assignments: assignments,
                table: table_name,
            },
        SqlQueryType::Select =>
            Query::Select {
                fields: fields,
                filter: filter_expression,
                joins: joins,
                limit: limit,
                order: order,
                table: table_name,
            },
        SqlQueryType::Update =>
            Query::Update {
                assignments: assignments,
                filter: filter_expression,
                table: table_name,
            },
    }
}

/// Create an error about a table not having a primary key.
fn no_primary_key(table_name: &str, position: Span) -> Error {
    Error::new(format!("Table {} does not have a primary key", table_name), position)
}

/// Gather data about the query in the method `calls`.
fn process_methods(calls: &[MethodCall], table: &SqlFields, table_name: &str) -> SqlResult<QueryData> {
    let mut errors = vec![];
    let mut assignments = vec![];
    let mut filter_expression = FilterExpression::NoFilters;
    let mut joins = vec![];
    let mut limit = Limit::NoLimit;
    let mut order = vec![];
    let mut query_type = SqlQueryType::Select;
    let mut typed_fields = vec![];
    for method_call in calls {
        match &method_call.name[..] {
            "all" => {
                check_no_arguments(&method_call, &mut errors);
            },
            "create" => {
                check_no_arguments(&method_call, &mut errors);
                query_type = SqlQueryType::CreateTable;
                for (field, typ) in table {
                    typed_fields.push(TypedField {
                        identifier: field.clone(),
                        typ: typ.node.to_sql(),
                    });
                }
            },
            "delete" => {
                check_no_arguments(&method_call, &mut errors);
                query_type = SqlQueryType::Delete;
            },
            "drop" => {
                check_no_arguments(&method_call, &mut errors);
                query_type = SqlQueryType::Drop;
            },
            "filter" => {
                try(expression_to_filter_expression(&method_call.arguments[0], &table_name, table), &mut errors, |filter| {
                    filter_expression = filter;
                });
            },
            "get" => {
                // TODO: la méthode get() accepte d’être utilisée sans argument.
                try(get_expression_to_filter_expression(&method_call.arguments[0], &table_name, table), &mut errors, |(filter, new_limit)| {
                    filter_expression = filter;
                    limit = new_limit;
                });
            },
            "insert" => {
                try(convert_arguments(&method_call.arguments, &table_name, table, argument_to_assignment), &mut errors, |assigns| {
                    assignments = assigns;
                });
                check_insert_arguments(&assignments, method_call.position, &table, &mut errors);
                query_type = SqlQueryType::Insert;
            },
            "join" => {
                try(convert_arguments(&method_call.arguments, &table_name, table, argument_to_join), &mut errors, |mut new_joins| {
                    joins.append(&mut new_joins);
                });
            },
            "limit" => {
                try(arguments_to_limit(&method_call.arguments[0]), &mut errors, |new_limit| {
                    limit = new_limit;
                });
            },
            "sort" => {
                try(convert_arguments(&method_call.arguments, &table_name, table, argument_to_order), &mut errors, |new_order| {
                    order = new_order;
                });
            },
            "update" => {
                try(convert_arguments(&method_call.arguments, &table_name, table, argument_to_assignment), &mut errors, |assigns| {
                    assignments = assigns;
                });
                query_type = SqlQueryType::Update;
            },
            _ => (), // Nothing to do since check_methods() check for unknown method.
        }
    }
    res((filter_expression, joins, limit, order, assignments, typed_fields, query_type), errors)
}

/// If `result` is an `Err`, add the errors to `errors`.
/// Otherwise, execute the closure.
fn try<F: FnMut(T), T>(mut result: Result<T, Vec<Error>>, errors: &mut Vec<Error>, mut fn_using_result: F) {
    match result {
        Ok(value) => fn_using_result(value),
        Err(ref mut errs) => errors.append(errs),
    }
}

fn unknown_method(position: Span, object_type: &Type, method_name: String, type_methods: Option<&HashMap<String, String>>, errors: &mut Vec<Error>) {
    errors.push(Error::new(
        format!("no method named `{}` found for type `{}`", method_name, object_type), // TODO: améliorer ce message.
        position,
    ));
    if let Some(type_methods) = type_methods {
        if let Some(name) = find_near(&method_name, type_methods.keys()) {
            errors.push(Error::new_help(
                format!("did you mean {}?", name),
                position,
            ));
        }
    }
}

/// Add an error to the vector error about an unknown SQL table.
/// It suggests a similar name if there exist one.
pub fn unknown_table_error(table_name: &str, position: Span, sql_tables: &SqlTables, errors: &mut Vec<Error>) {
    errors.push(Error::new_with_code(
        format!("`{}` does not name an SQL table", table_name),
        position,
        "E0422",
    ));
    let tables = sql_tables.keys();
    if let Some(name) = find_near(&table_name, tables) {
        errors.push(Error::new_help(
            format!("did you mean {}?", name),
            position,
        ));
    }
    else {
        errors.push(Error::new_help(
            format!("did you forget to add the #[sql_table] attribute on the {} struct?", table_name),
            position,
        ));
    }
}
