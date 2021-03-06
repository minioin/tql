/*
 * Copyright (c) 2018 Boucher, Antoni <bouanto@zoho.com>
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy of
 * this software and associated documentation files (the "Software"), to deal in
 * the Software without restriction, including without limitation the rights to
 * use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
 * the Software, and to permit persons to whom the Software is furnished to do so,
 * subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
 * FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR
 * COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER
 * IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
 * CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
 */

//! The SQLite code generator.

use proc_macro2::TokenStream ;

use ast::Aggregate;
use sql::{SqlBackend, ToSql};

pub struct DummySqlBackend {}

pub fn create_sql_backend() -> DummySqlBackend {
    DummySqlBackend { }
}

impl ToSql for Aggregate {
    fn to_sql(&self, _index: &mut usize) -> String {
        unreachable!("Enable one of the following features: sqlite, pg");
    }
}

impl SqlBackend for DummySqlBackend {
    fn insert_query(&self, _table: &str, _fields: &[String], _values: &[String]) -> TokenStream {
        unreachable!("Enable one of the following features: sqlite, pg");
    }
}
