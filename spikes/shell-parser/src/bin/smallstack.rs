//! The guard limit must hold on a 2 MiB thread stack (Tokio's worker default).
fn main() {
    let limit = shell_parser_spike::MAX_NESTING;
    let inputs = [
        format!("{}ls;{}", "{ ".repeat(limit), " }".repeat(limit)),
        format!("{}ls{}", "( ".repeat(limit), " )".repeat(limit)),
        format!("{}ls{}", "$(".repeat(30), ")".repeat(30)),
        format!("{}ls{}", "if true; then ".repeat(limit / 2), "; fi".repeat(limit / 2)),
    ];
    for input in inputs {
        let handle = std::thread::Builder::new().stack_size(2 * 1024 * 1024).spawn(move || shell_parser_spike::split(&input).map(|c| c.len())).unwrap();
        println!("{:?}", handle.join().map_err(|_| "thread panicked"));
    }
}
