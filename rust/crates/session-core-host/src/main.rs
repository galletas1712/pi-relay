use std::io::{stdin, stdout};

fn main() -> std::io::Result<()> {
	session_core_host::run_stdio(stdin().lock(), stdout().lock())
}
