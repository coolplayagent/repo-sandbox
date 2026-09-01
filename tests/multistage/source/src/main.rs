fn answer() -> u8 {
    42
}

fn main() {
    println!("{}", answer());
}

#[cfg(test)]
mod tests {
    #[test]
    fn rust_toolchain_executes_tests() {
        assert_eq!(super::answer(), 42);
    }
}
