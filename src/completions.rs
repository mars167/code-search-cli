use crate::cli::CompletionShell;

const COMMANDS: &[&str] = &[
    "find",
    "grep",
    "files",
    "find-path",
    "findpath",
    "path",
    "glob",
    "list",
    "ls",
    "tree",
    "read",
    "refs",
    "symbols",
    "defs",
    "calls",
    "callers",
    "changed",
    "status",
    "watch",
    "serve",
    "index",
    "hooks",
    "completions",
];

pub fn script(shell: &CompletionShell) -> String {
    match shell {
        CompletionShell::Bash => bash(),
        CompletionShell::Zsh => zsh(),
        CompletionShell::Fish => fish(),
    }
}

fn bash() -> String {
    let commands = COMMANDS.join(" ");
    format!(
        r#"_code_search()
{{
  local cur prev commands index_cmds hooks_cmds shells
  COMPREPLY=()
  cur="${{COMP_WORDS[COMP_CWORD]}}"
  prev="${{COMP_WORDS[COMP_CWORD-1]}}"
  commands="{commands}"
  index_cmds="build update status verify clean import-scip"
  hooks_cmds="install uninstall status"
  shells="bash zsh fish"

  case "$prev" in
    index)
      COMPREPLY=( $(compgen -W "$index_cmds" -- "$cur") )
      return 0
      ;;
    hooks)
      COMPREPLY=( $(compgen -W "$hooks_cmds" -- "$cur") )
      return 0
      ;;
    completions)
      COMPREPLY=( $(compgen -W "$shells" -- "$cur") )
      return 0
      ;;
  esac

  if [[ "$cur" == -* ]]; then
    COMPREPLY=( $(compgen -W "--path --output --include --exclude --hidden --no-ignore --lang --changed --cursor --limit --context --help --version" -- "$cur") )
  else
    COMPREPLY=( $(compgen -W "$commands" -- "$cur") )
  fi
}}
complete -F _code_search code-search
"#
    )
}

fn zsh() -> String {
    let commands = COMMANDS.join(" ");
    format!(
        r#"#compdef code-search

_code_search() {{
  local -a commands index_cmds hooks_cmds shells
  commands=({commands})
  index_cmds=(build update status verify clean import-scip)
  hooks_cmds=(install uninstall status)
  shells=(bash zsh fish)

  if (( CURRENT == 2 )); then
    _describe 'command' commands
    return
  fi

  case $words[2] in
    index)
      _describe 'index command' index_cmds
      ;;
    hooks)
      _describe 'hook command' hooks_cmds
      ;;
    completions)
      _describe 'shell' shells
      ;;
    *)
      _files
      ;;
  esac
}}

_code_search "$@"
"#
    )
}

fn fish() -> String {
    let mut lines = vec![
        "complete -c code-search -f".to_string(),
        "complete -c code-search -l path -r".to_string(),
        "complete -c code-search -l output -xa 'json compact-json jsonl text'".to_string(),
        "complete -c code-search -l include -r".to_string(),
        "complete -c code-search -l exclude -r".to_string(),
        "complete -c code-search -l hidden".to_string(),
        "complete -c code-search -l no-ignore".to_string(),
        "complete -c code-search -l lang -r".to_string(),
        "complete -c code-search -l changed".to_string(),
        "complete -c code-search -l cursor -r".to_string(),
        "complete -c code-search -l limit -r".to_string(),
        "complete -c code-search -l context -r".to_string(),
    ];
    for command in COMMANDS {
        lines.push(format!("complete -c code-search -f -a {command}"));
    }
    lines.push("complete -c code-search -n '__fish_seen_subcommand_from index' -a 'build update status verify clean import-scip'".to_string());
    lines.push("complete -c code-search -n '__fish_seen_subcommand_from hooks' -a 'install uninstall status'".to_string());
    lines.push(
        "complete -c code-search -n '__fish_seen_subcommand_from completions' -a 'bash zsh fish'"
            .to_string(),
    );
    lines.push(String::new());
    lines.join("\n")
}
