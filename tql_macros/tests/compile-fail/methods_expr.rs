//! Tests of the type analyzer lint for a `Query::Select` using the methods available in the
//! filter() method.

#![feature(plugin)]
#![plugin(tql_macros)]

extern crate postgres;
extern crate tql;

use postgres::{Connection, SslMode};
use tql::PrimaryKey;

#[SqlTable]
#[derive(Debug)]
struct Table {
    id: PrimaryKey,
    field1: String,
    i32_field: i32,
}

fn get_connection() -> Connection {
    Connection::connect("postgres://test:test@localhost/database", &SslMode::None).unwrap()
}

fn main() {
    let connection = get_connection();

    let value = 42;
    sql!(Table.filter(field1.contains(value) == true));
    //~^ ERROR mismatched types:
    //~| expected `String`,
    //~| found `i32` [E0308]
    //~| HELP run `rustc --explain E0308` to see a detailed explanation
    //~| NOTE in this expansion of sql! (defined in tql)
}
