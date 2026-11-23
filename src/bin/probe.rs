fn main() {
    let data = b"hello world hello world hello world hello world".to_vec();
    let packed = zap::encode_block(&data, 6, 1);
    println!(
        "in={} packed={} first8={:02x?}",
        data.len(),
        packed.len(),
        &packed[..8.min(packed.len())]
    );
    match std::panic::catch_unwind(|| zap::decode_block(&packed, data.len(), 6, 1)) {
        Ok(v) => println!("decoded {} bytes equal={}", v.len(), v == data),
        Err(_) => println!("decode panicked"),
    }
}
