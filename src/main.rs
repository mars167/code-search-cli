use clap::{error::ErrorKind, Parser};
use code_search_cli::{cli::Cli, cli::OutputFormat, commands, output};

fn main() {
    let parse_error_format = requested_output_format(std::env::args().skip(1));
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                print!("{error}");
                std::process::exit(0);
            }
            let value = output::error_response_with_code("cli_usage_error", error.to_string());
            if output::emit(&parse_error_format, &value).is_err() {
                eprintln!("failed to render error response");
            }
            std::process::exit(1);
        }
    };
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

fn requested_output_format(args: impl IntoIterator<Item = String>) -> OutputFormat {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--output" {
            return args
                .next()
                .and_then(|value| output_format_from_str(&value))
                .unwrap_or(OutputFormat::Text);
        }
        if let Some(value) = arg.strip_prefix("--output=") {
            return output_format_from_str(value).unwrap_or(OutputFormat::Text);
        }
    }
    OutputFormat::Text
}

fn output_format_from_str(value: &str) -> Option<OutputFormat> {
    match value {
        "json" => Some(OutputFormat::Json),
        "compact-json" => Some(OutputFormat::CompactJson),
        "jsonl" => Some(OutputFormat::Jsonl),
        "text" => Some(OutputFormat::Text),
        _ => None,
    }
}
