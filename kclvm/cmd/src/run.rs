use anyhow::Result;
use clap::ArgMatches;
use kclvm_error::StringError;
use kclvm_parser::ParseSession;
use kclvm_runner::exec_program;
use std::sync::Arc;

use crate::settings::must_build_settings;

/// Run the KCL run command.
pub fn run_command(matches: &ArgMatches) -> Result<()> {
    // Config settings building
    let settings = must_build_settings(matches);
    let output = settings.output();
    let sess = Arc::new(ParseSession::default());
    match exec_program(sess.clone(), &settings.try_into()?) {
        Ok(result) => match output {
            Some(o) => {
                std::fs::write(o, result.yaml_result).unwrap();
            }
            None => println!("{}", result.yaml_result),
        },
        Err(msg) => {
            if !sess.0.diag_handler.has_errors()? {
                sess.0.add_err(StringError(msg))?;
            }
            sess.0.emit_stashed_diagnostics_and_abort()?;
        }
    }
    Ok(())
}
