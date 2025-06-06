// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

use typespec_client_core::fmt::SafeDebug;

#[derive(SafeDebug)]
pub struct Tuple(pub i32, pub &'static str);

#[derive(SafeDebug)]
pub struct EmptyTuple();

#[derive(SafeDebug)]
pub struct Struct {
    pub a: i32,
    pub b: &'static str,
}

#[derive(SafeDebug)]
pub struct EmptyStruct {}

#[derive(SafeDebug)]
pub struct UnitStruct;

#[derive(SafeDebug)]
pub enum Enum {
    Unit,
    Tuple(i32, &'static str),
    EmptyTuple(),
    Struct { a: i32, b: &'static str },
    EmptyStruct {},
}
