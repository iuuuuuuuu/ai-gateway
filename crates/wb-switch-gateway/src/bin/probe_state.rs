use wb_switch_gateway::pool::{Pool, StateFile};
fn main() {
    let p = r"D:\workbuddy2api\_ab\fixture\data\state.json";
    let raw = std::fs::read(p).unwrap();
    println!("bytes={}", raw.len());
    match serde_json::from_slice::<StateFile>(&raw) {
        Ok(sf) => println!("OK accounts={}", sf.accounts.len()),
        Err(e) => println!("ERR {e}"),
    }
}
