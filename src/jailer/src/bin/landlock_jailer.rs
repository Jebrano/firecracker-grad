// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Landlock ABI V7-based jailer. Shares all orchestration logic with `jailer` (see
//! `src/lib.rs` and `src/env.rs`) -- this binary only picks which filesystem isolation
//! backend `Env::run()` applies. See `src/landlock.rs` for the ruleset itself.

use jailer::JailerError;
use jailer::env::Isolation;

fn main() -> Result<(), JailerError> {
    jailer::run(Isolation::Landlock, "Landlock Jailer")
}
