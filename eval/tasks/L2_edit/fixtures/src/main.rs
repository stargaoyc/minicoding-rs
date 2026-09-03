// 预期行为：add(a, b) 返回 a + b；当前实现有 bug（返回 a - b）。
fn add(a: i32, b: i32) -> i32 {
    a - b // BUG：应为 a + b
}

fn main() {
    println!("{}", add(2, 3));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn add_works() {
        assert_eq!(add(2, 3), 5);
        assert_eq!(add(-1, 1), 0);
    }
}
