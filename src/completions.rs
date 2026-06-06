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
    "query",
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
        r#"_codetrail()
{{
  local cur prev commands query_cmds index_cmds hooks_cmds shells
  COMPREPLY=()
  cur="${{COMP_WORDS[COMP_CWORD]}}"
  prev="${{COMP_WORDS[COMP_CWORD-1]}}"
  commands="{commands}"
  query_cmds="replay show list delete"
  index_cmds="build update status verify clean import-scip generate-scip"
  hooks_cmds="install uninstall status"
  shells="bash zsh fish"

  case "$prev" in
    query)
      COMPREPLY=( $(compgen -W "$query_cmds" -- "$cur") )
      return 0
      ;;
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
    COMPREPLY=( $(compgen -W "--path --output --include --exclude --hidden --no-ignore --lang --changed --cursor --allow-broad --limit --context --save-query --help --version" -- "$cur") )
  else
    COMPREPLY=( $(compgen -W "$commands" -- "$cur") )
  fi
}}
complete -F _codetrail codetrail
"#
    )
}

fn zsh() -> String {
    let commands = COMMANDS.join(" ");
    format!(
        r#"#compdef codetrail

_codetrail() {{
  local -a commands query_cmds index_cmds hooks_cmds shells global_opts
  commands=({commands})
  query_cmds=(replay show list delete)
  index_cmds=(build update status verify clean import-scip)
  hooks_cmds=(install uninstall status)
  shells=(bash zsh fish)
  global_opts=(--path --output --include --exclude --hidden --no-ignore --lang --changed --cursor --allow-broad --limit --context --save-query --help --version)

  if [[ "$words[CURRENT]" == -* ]]; then
    _describe 'option' global_opts
    return
  fi

  if (( CURRENT == 2 )); then
    _describe 'command' commands
    return
  fi

  case $words[2] in
    query)
      _describe 'query command' query_cmds
      ;;
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

_codetrail "$@"
"#
    )
}

fn fish() -> String {
    let mut lines = vec![
        "complete -c codetrail -f".to_string(),
        "complete -c codetrail -l path -r".to_string(),
        "complete -c codetrail -l output -xa 'json compact-json jsonl text'".to_string(),
        "complete -c codetrail -l include -r".to_string(),
        "complete -c codetrail -l exclude -r".to_string(),
        "complete -c codetrail -l hidden".to_string(),
        "complete -c codetrail -l no-ignore".to_string(),
        "complete -c codetrail -l lang -r".to_string(),
        "complete -c codetrail -l changed".to_string(),
        "complete -c codetrail -l cursor -r".to_string(),
        "complete -c codetrail -l allow-broad".to_string(),
        "complete -c codetrail -l limit -r".to_string(),
        "complete -c codetrail -l context -r".to_string(),
        "complete -c codetrail -l save-query -r".to_string(),
    ];
    for command in COMMANDS {
        lines.push(format!("complete -c codetrail -f -a {command}"));
    }
    lines.push(
        "complete -c codetrail -n '__fish_seen_subcommand_from query' -a 'replay show list delete'"
            .to_string(),
    );
    lines.push("complete -c codetrail -n '__fish_seen_subcommand_from index' -a 'build update status verify clean import-scip'".to_string());
    lines.push("complete -c codetrail -n '__fish_seen_subcommand_from hooks' -a 'install uninstall status'".to_string());
    lines.push(
        "complete -c codetrail -n '__fish_seen_subcommand_from completions' -a 'bash zsh fish'"
            .to_string(),
    );
    lines.push(String::new());
    lines.join("\n")
}
