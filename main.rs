//! The engine binary — argv[1] names the component, argv[2] carries
//! the args JSON (the windmill bash-script `$1` shape); the result
//! prints to stdout as JSON, errors exit non-zero (the job failure
//! the platform sees). See [`godon_engine::components`] for the
//! dispatch itself and its receipts.

use std::io::Read;

mod invocation;
mod router;

use crate::invocation::{emit_result, run_component, ComponentDeps};
use crate::router::run_platform_job;

fn main() {
    let mut argv = std::env::args().skip(1);
    let component = match argv.next() {
        Some(c) => c,
        None => match std::env::var("WM_JOB_PATH") {
            // the platform run: an empty-body sandbox script — the
            // rail's reserved env carries the routing (WM_JOB_PATH)
            // and the payload source (WM_JOB_ID); the binary is the
            // entrypoint AND the router, no wrapper anywhere
            Ok(_) => {
                let deps = match ComponentDeps::production() {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("component dependencies failed: {e}");
                        std::process::exit(2);
                    }
                };
                match run_platform_job(&deps) {
                    Ok(result) => emit_result(&result),
                    Err(e) => {
                        eprintln!("{e}");
                        std::process::exit(1);
                    }
                }
                return;
            }
            Err(_) => {
                eprintln!(
                    "usage: godon-engine <worker|recon-http|recon-prometheus|effectuation-http|effectuation-ssh|preflight> [args-json]"
                );
                std::process::exit(2);
            }
        },
    };
    let args_raw = match argv.next() {
        Some(a) => a,
        None => {
            // no argv JSON — read stdin (direct invocation shape)
            let mut buf = String::new();
            let _ = std::io::stdin().read_to_string(&mut buf);
            buf
        }
    };
    let args: serde_json::Value = match serde_json::from_str(&args_raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("invalid args json: {e}");
            std::process::exit(2);
        }
    };

    let deps = match ComponentDeps::production() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("component dependencies failed: {e}");
            std::process::exit(2);
        }
    };

    match run_component(&component, &args, &deps) {
        Ok(result) => emit_result(&result),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
