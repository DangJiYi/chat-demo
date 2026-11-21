//! 计费：峰谷判定与价格。
//!
//! 口径必须与 server.py 完全一致——两边算出的金额不同，用户会立刻发现。
//! 常量（时段、节假日、价目）集中在这里，改的时候两边一起改。
//!
//! 不引 chrono：只需要「北京时间的年月日时分 + 星期几」，
//! Howard Hinnant 的 civil_from_days 算法约 20 行且精确，
//! 比多一个依赖（约 200–300 KB）划算。

/// 峰时时段（北京时间，左闭右开）
const PEAK_HOURS: [(u32, u32); 2] = [(9, 12), (14, 18)];

/// 法定节假日（北京时间日期，YYYY-MM-DD）。官方每年公布后更新。
const HOLIDAYS: &[&str] = &[
    "2026-01-01", "2026-02-16", "2026-02-17", "2026-02-18", "2026-02-19", "2026-02-20",
    "2026-02-21", "2026-02-22", "2026-04-04", "2026-04-05", "2026-04-06", "2026-05-01",
    "2026-05-02", "2026-05-03", "2026-05-04", "2026-05-05", "2026-06-19", "2026-06-20",
    "2026-06-21", "2026-09-25", "2026-09-26", "2026-09-27", "2026-10-01", "2026-10-02",
    "2026-10-03", "2026-10-04", "2026-10-05", "2026-10-06", "2026-10-07",
];

/// 价目（元 / 百万 tokens，峰价）：(缓存命中, 未命中, 输出)
fn prices(model: &str) -> (f64, f64, f64) {
    match model {
        "deepseek-v4-pro" => (0.30, 9.00, 27.00),
        // 未记录的模型一律按 flash 算，与 Python 版 PRICES.get(..., flash) 一致
        _ => (0.04, 2.00, 8.00),
    }
}

/// 北京时间偏移：UTC+8，无夏令时。
const BJ_OFFSET_SECS: i64 = 8 * 3600;

/// 从「1970-01-01 起的天数」推回 (年, 月, 日)。
///
/// Howard Hinnant, "chrono-Compatible Low-Level Date Algorithms"。
/// 适用于 1970 前后各数千年，对我们足够。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// 时间戳（毫秒）→ 北京时间 (年, 月, 日, 时, 分)
fn bj_parts(ms: i64) -> (i64, u32, u32, u32, u32) {
    let secs = ms.div_euclid(1000) + BJ_OFFSET_SECS;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (y, m, d, (sod / 3600) as u32, ((sod % 3600) / 60) as u32)
}

/// 星期几：周一=0 … 周日=6（与 Python 的 datetime.weekday() 一致）
fn weekday(ms: i64) -> u32 {
    let days = (ms.div_euclid(1000) + BJ_OFFSET_SECS).div_euclid(86_400);
    (days + 3).rem_euclid(7) as u32
}

/// YYYY-MM-DD，用于查节假日表与按日/按月分桶。
pub fn bj_date_key(ms: i64) -> String {
    let (y, m, d, _, _) = bj_parts(ms);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DD HH:MM`（北京时间），用于日志时间戳。
pub fn bj_time_key(ms: i64) -> String {
    let (y, m, d, hh, mm) = bj_parts(ms);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
}

/// YYYY-MM，用于按月分桶。
pub fn bj_month_key(ms: i64) -> String {
    let (y, m, _, _, _) = bj_parts(ms);
    format!("{y:04}-{m:02}")
}

/// 是否峰时。
///
/// 规则：节假日 → 谷；周末 → 谷（调休上班日不特殊处理）；其余看时段。
pub fn is_peak(ms: i64) -> bool {
    if HOLIDAYS.contains(&bj_date_key(ms).as_str()) {
        return false;
    }
    if weekday(ms) >= 5 {
        return false;
    }
    let (_, _, _, hour, _) = bj_parts(ms);
    PEAK_HOURS.iter().any(|(a, b)| *a <= hour && hour < *b)
}

/// 下一个峰谷切换点（毫秒，整分钟对齐）。
pub fn next_boundary(ms: i64) -> i64 {
    let cur = is_peak(ms);
    let mut t = ms - ms.rem_euclid(60_000) + 60_000;
    for _ in 0..(8 * 24 * 60) {
        if is_peak(t) != cur {
            return t;
        }
        t += 60_000;
    }
    t
}

/// 上一个峰谷切换点。
pub fn prev_boundary(ms: i64) -> i64 {
    let cur = is_peak(ms);
    let mut t = ms - ms.rem_euclid(60_000);
    for _ in 0..(7 * 24 * 60) {
        let p = t - 60_000;
        if is_peak(p) != cur {
            return t;
        }
        t = p;
    }
    t
}

/// 本次调用费用（元）。谷时 = 峰价 × 0.5。
pub fn cost_of(model: &str, ms: i64, hit: i64, miss: i64, out: i64) -> f64 {
    let (ph, pm, po) = prices(model);
    let factor = if is_peak(ms) { 1.0 } else { 0.5 };
    (hit as f64 * ph + miss as f64 * pm + out as f64 * po) * factor / 1e6
}

/// 同样的用量全按谷价算要多少钱（用于算「错峰省了多少」）。
pub fn offpeak_cost(model: &str, hit: i64, miss: i64, out: i64) -> f64 {
    let (ph, pm, po) = prices(model);
    (hit as f64 * ph + miss as f64 * pm + out as f64 * po) * 0.5 / 1e6
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_match_known_values() {
        // 1970-01-01 → epoch 第 0 天
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2000-03-01 → 第 11017 天（闰年边界，最容易算错的那天）
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        // 2026-11-21 → 第 20778 天
        assert_eq!(civil_from_days(20_778), (2026, 11, 21));
    }

    #[test]
    fn weekday_matches_python_convention() {
        // 1970-01-01 是周四 → Python weekday() = 3
        assert_eq!(weekday(0), 3);
        // 2026-11-21 是周六 → 5（周一=0）
        const DAY: i64 = 86_400_000;
        assert_eq!(weekday(20_778 * DAY), 5);
        // 2026-11-20 是周五 → 4
        assert_eq!(weekday(20_777 * DAY), 4);
    }

    #[test]
    fn peak_rules() {
        const DAY: i64 = 86_400_000;
        // 2026-11-20 周五：北京时间 10:00 = UTC 02:00
        let fri_10_bj = 20_777 * DAY + 2 * 3_600_000;
        assert!(is_peak(fri_10_bj), "周五 10 点应为峰时");
        // 同一天 13:00 北京 = 05:00 UTC（午休）
        let fri_13_bj = 20_777 * DAY + 5 * 3_600_000;
        assert!(!is_peak(fri_13_bj), "13 点应为谷时");
        // 周六 10:00 北京 = 02:00 UTC，周末全谷
        let sat_10_bj = 20_778 * DAY + 2 * 3_600_000;
        assert!(!is_peak(sat_10_bj), "周六全天应为谷时");
    }

    #[test]
    fn cost_is_halved_off_peak() {
        const DAY: i64 = 86_400_000;
        let peak_ts = 20_777 * DAY + 2 * 3_600_000;
        let off_ts = 20_777 * DAY + 5 * 3_600_000;
        let a = cost_of("deepseek-flash", peak_ts, 0, 1_000_000, 0);
        let b = cost_of("deepseek-flash", off_ts, 0, 1_000_000, 0);
        assert!((a - 2.0).abs() < 1e-9, "峰时 1M 未命中应 2 元，实际 {a}");
        assert!((b - 1.0).abs() < 1e-9, "谷时应 1 元，实际 {b}");
    }
}
