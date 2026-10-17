//! The engine binary — argv[1] names the component, argv[2] carries
//! the args JSON (the windmill bash-script `$1` shape); the result
//! prints to stdout as JSON, errors exit non-zero (the job failure
//! the platform sees). See [`godon_engine::components`] for the
//! dispatch itself and its receipts.

use std::io::Read;

use godon_engine::components::{run_component, ComponentDeps};

fn main() {
    let mut argv = std::env::args().skip(1);
    let component = match argv.next() {
        Some(c) => c,
        None => {
            eprintln!(
                "usage: godon-engine <worker|recon-http|recon-prometheus|effectuation-http|effectuation-ssh> [args-json]"
            );
            std::process::exit(2);
        }
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
        Ok(result) => {
            println!("{result}");
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
