// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Default, chroot-based jailer. Shares all orchestration logic with `landlock-jailer`
//! (see `src/lib.rs` and `src/env.rs`) -- this binary only picks which filesystem
//! isolation backend `Env::run()` applies.

use jailer::JailerError;
use jailer::env::Isolation;

fn main() -> Result<(), JailerError> {
    jailer::run(Isolation::Chroot, "Jailer")
}
