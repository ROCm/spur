// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Terminate argument parsing through clap instead of `anyhow`.
//!
//! Clap reports `--help` as an `Err(clap::Error)`. Propagating that error with
//! `?` turns it into an `anyhow::Error`, which `main` prints as
//! `Error: <help text>` on stderr and exits with code 1. Routing it through
//! `clap::Error::exit` instead prints help to stdout with code 0, and genuine
//! parse errors to stderr with code 2 — matching every standard CLI.
//!
//! `--version` is not registered on the subcommand parsers, so it is not
//! handled here; the top-level `spur --version` path lives in `main`.

use clap::{ArgMatches, Command, FromArgMatches, Parser};

/// Parse a clap `Parser`, exiting cleanly on `--help` or parse errors.
pub fn parse_or_exit<T: Parser>(args: &[String]) -> T {
    T::try_parse_from(args).unwrap_or_else(|e| e.exit())
}

/// Resolve `ArgMatches` for callers that need the raw matches (e.g. to read
/// argument sources for env-default resolution), exiting cleanly like
/// [`parse_or_exit`].
pub fn matches_or_exit(command: Command, args: &[String]) -> ArgMatches {
    command
        .try_get_matches_from(args)
        .unwrap_or_else(|e| e.exit())
}

/// Build a typed args struct from resolved [`ArgMatches`], exiting cleanly on
/// error. Unlike parsing, `from_arg_matches` never yields help/version, so this
/// only guards against structural conversion failures.
pub fn from_matches_or_exit<T: FromArgMatches>(matches: &ArgMatches) -> T {
    T::from_arg_matches(matches).unwrap_or_else(|e| e.exit())
}
