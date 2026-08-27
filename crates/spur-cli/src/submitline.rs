// SPDX-License-Identifier: Apache-2.0

//! Renders the submit command line recorded on a job for `scontrol show job`.

/// Join argv into a single displayable line, quoting only the arguments that
/// would otherwise be ambiguous. Slurm leaves these unquoted, which loses the
/// word boundaries of `--wrap "a b"`.
pub fn render(args: &[String]) -> String {
    args.iter()
        .map(|arg| quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }
    if !arg.chars().any(needs_quoting) {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

fn needs_quoting(c: char) -> bool {
    c.is_whitespace() || "'\"\\$`|&;<>()*?[]{}!#~".contains(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn plain_args_are_joined_unquoted() {
        assert_eq!(
            render(&v(&["sbatch", "-w", "node1", "--exclusive", "job.sh"])),
            "sbatch -w node1 --exclusive job.sh"
        );
    }

    #[test]
    fn args_with_spaces_are_quoted() {
        assert_eq!(
            render(&v(&["sbatch", "--wrap", "hostname; sleep 5"])),
            "sbatch --wrap 'hostname; sleep 5'"
        );
    }

    #[test]
    fn embedded_single_quotes_are_escaped() {
        assert_eq!(render(&v(&["srun", "echo it's"])), r"srun 'echo it'\''s'");
    }

    #[test]
    fn empty_arg_is_preserved_as_empty_quotes() {
        assert_eq!(
            render(&v(&["sbatch", "--comment", ""])),
            "sbatch --comment ''"
        );
    }

    #[test]
    fn shell_metacharacters_are_quoted_even_without_spaces() {
        assert_eq!(render(&v(&["srun", "a|b"])), "srun 'a|b'");
        assert_eq!(render(&v(&["srun", "$HOME"])), "srun '$HOME'");
    }

    #[test]
    fn hostlist_brackets_are_quoted() {
        assert_eq!(
            render(&v(&["sbatch", "-w", "node[1-4]"])),
            "sbatch -w 'node[1-4]'"
        );
    }
}
