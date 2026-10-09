//! Console-subsystem front for the pack CLI — waits, prints, exits like a tool should.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match punktfunk_setup_win::pack::run(&args) {
        Ok(report) => println!("{report}"),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    }
}
