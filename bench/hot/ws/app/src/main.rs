fn main() {
    pliron_hot::start();
    for t in 0.. {
        let v = leaf::probe(t);
        let c = leaf::counter_bump();
        let s = leaf::total(t);
        println!("tick {t} probe {v} ctr {c} total {s}");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
