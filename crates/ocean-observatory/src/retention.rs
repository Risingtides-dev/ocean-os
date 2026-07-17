#[derive(Debug, Clone, Copy)]
pub struct RetentionPolicy {
    pub max_age_days: u64,
    pub max_bytes: u64,
}
impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_age_days: 7,
            max_bytes: 1024 * 1024 * 1024,
        }
    }
}
