use clap::Parser;
use code_search_cli::{cli::Cli, commands, output};

fn main() {
    let cli = Cli::parse();
    let output = cli.output.clone();

    let exit_code = match commands::run(cli) {
        Ok(code) => code,
        Err(error) => {
            let value = output::error_response(error);
            if output::emit(&output, &value).is_err() {
                eprintln!("failed to render error response");
            }
            1
        }
    };

    std::process::exit(exit_code);
}
