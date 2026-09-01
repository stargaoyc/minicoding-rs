// issue: 需要实现函数 sum(xs: &[i32]) -> i32 返回切片元素和。
// 当前实现返回 0（占位，测试失败）。
pub fn sum(xs: &[i32]) -> i32 {
    0 // TODO: 实现
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sum_works() {
        assert_eq!(sum(&[1, 2, 3]), 6);
        assert_eq!(sum(&[]), 0);
        assert_eq!(sum(&[-1, 1]), 0);
    }
}
