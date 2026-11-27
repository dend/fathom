// The command line tool. It is its own program because the app is built as a
// windowed program on Windows, which can't write to a terminal or report an
// exit code to one.

fn main() {
    std::process::exit(fathom::cli::main());
}
