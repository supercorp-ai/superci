//! What io.rs gives every runtime: a store lent, a swap in two steps, ends kept beside records (`Ending`), and what a
//! machine is told; each with the store failing at every step it takes.
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use async_trait::async_trait;
use futures::executor::block_on;
use superci_core::io::{swap_in_two_steps, Ending, Result, Store, Work};

/// Records in memory; the operation numbered `fail_at` (counted from 0) fails.
#[derive(Default)]
struct Memory { kept: RefCell<BTreeMap<String, String>>, ops: Cell<usize>, fail_at: Cell<Option<usize>> }

impl Memory {
    fn step(&self) -> Result<()> {
        let n = self.ops.get();
        self.ops.set(n + 1);
        if self.fail_at.get() == Some(n) { return Err(format!("operation {n} failed")) }
        Ok(())
    }
    fn keys(&self) -> Vec<String> { self.kept.borrow().keys().cloned().collect() }
}

#[async_trait(?Send)]
impl Store for Memory {
    async fn get(&self, key: &str) -> Result<Option<String>> { self.step()?; Ok(self.kept.borrow().get(key).cloned()) }
    async fn put(&self, key: &str, value: String) -> Result<()> { self.step()?; self.kept.borrow_mut().insert(key.into(), value); Ok(()) }
    async fn put_if_absent(&self, key: &str, value: String) -> Result<bool> {
        self.step()?;
        let mut kept = self.kept.borrow_mut();
        if kept.contains_key(key) { return Ok(false) }
        kept.insert(key.into(), value);
        Ok(true)
    }
    async fn delete(&self, key: &str) -> Result<()> { self.step()?; self.kept.borrow_mut().remove(key); Ok(()) }
    async fn list(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        self.step()?;
        Ok(self.kept.borrow().iter().filter(|(k, _)| k.starts_with(prefix)).map(|(k, v)| (k.clone(), v.clone())).collect())
    }
}

/// Every store operation through a store lent (`&T`), as the control plane passes its store around.
async fn through<S: Store>(s: S) -> Vec<String> {
    let mut said = vec![];
    s.put("job:1", "a".into()).await.unwrap();
    said.push(format!("{:?}", s.get("job:1").await.unwrap()));
    said.push(format!("{}", s.put_if_absent("job:2", "b".into()).await.unwrap()));
    said.push(format!("{}", s.swap("job:2", Some("b"), "c".into()).await.unwrap()));
    s.put_until("job:3", "d".into(), 5).await.unwrap();
    said.push(format!("{}", s.swap_until("job:3", Some("d"), "e".into(), Some(9)).await.unwrap()));
    said.push(format!("{:?}", s.list("job:").await.unwrap()));
    said.push(format!("{:?}", s.list_last("job:", 1).await.unwrap()));
    said.push(format!("{:?}", s.get_many(&["job:2".into(), "job:9".into()]).await.unwrap()));
    said.push(format!("{}", s.expire(100).await.unwrap()));
    said.push(format!("{}", s.migrate(10).await.unwrap()));
    s.delete("job:1").await.unwrap();
    said
}

#[test]
fn a_store_lent_is_the_same_store_and_a_store_with_ends_lent_too() {
    let memory = Memory::default();
    assert_eq!(block_on(through(&memory)), [
        r#"Some("a")"#, "true", "true", "true",
        r#"[("job:1", "a"), ("job:2", "c"), ("job:3", "e")]"#, r#"[("job:3", "e")]"#, r#"[Some("c"), None]"#, "0", "true",
    ]);
    assert_eq!(memory.keys(), ["job:2", "job:3"]);
    // With ends kept beside: the record given an end is removed once it is due.
    let ending = Ending(Memory::default());
    assert_eq!(block_on(through(&ending))[7], "1");
    assert_eq!(ending.0.keys(), ["job:2"]);
}

/// The last records, several at once and a typed one each say a failed read of the store as it is.
#[test]
fn reads_built_on_a_failing_read_say_its_failure() {
    let memory = Memory::default();
    block_on(memory.put("job:1", "{}".into())).unwrap();
    let failing = |m: &Memory| { m.fail_at.set(Some(m.ops.get())); format!("operation {} failed", m.ops.get()) };
    let said = failing(&memory);
    assert_eq!(block_on(memory.list_last("job:", 1)), Err(said));
    let said = failing(&memory);
    assert_eq!(block_on(memory.get_many(&["job:1".into()])), Err(said));
    let said = failing(&memory);
    assert_eq!(block_on(superci_core::io::get_json::<serde_json::Value>(&memory, "job:1")), Err(said));
    assert_eq!(block_on(superci_core::io::get_json::<serde_json::Value>(&memory, "job:1")), Ok(Some(serde_json::json!({}))));
}

#[test]
fn a_swap_in_two_steps_writes_only_over_what_was_read_and_a_failed_read_is_said() {
    let memory = Memory::default();
    block_on(memory.put("job:1", "a".into())).unwrap();
    assert!(!block_on(swap_in_two_steps(&memory, "job:1", Some("b"), "c".into())).unwrap());
    assert!(!block_on(swap_in_two_steps(&memory, "job:1", None, "c".into())).unwrap());
    assert_eq!(block_on(memory.get("job:1")).unwrap().as_deref(), Some("a"));
    memory.fail_at.set(Some(memory.ops.get()));
    assert_eq!(block_on(swap_in_two_steps(&memory, "job:1", Some("a"), "c".into())), Err("operation 4 failed".to_string()));
}

/// Runs `what` on a fresh store with ends, made by `given`, with its first operation failing, then its second, and so
/// on until it succeeds; each failure is given back as it is. How many operations it took.
fn failing_each_step<T: std::fmt::Debug>(given: impl Fn(&Ending<Memory>), what: impl Fn(&Ending<Memory>) -> Result<T>) -> usize {
    for k in 0.. {
        let store = Ending(Memory::default());
        given(&store);
        let start = store.0.ops.get();
        store.0.fail_at.set(Some(start + k));
        match what(&store) {
            Ok(_) => return k,
            Err(e) => assert_eq!(e, format!("operation {} failed", start + k)),
        }
    }
    unreachable!()
}

#[test]
fn a_store_with_ends_says_each_failed_step_of_each_write_and_of_its_expiry() {
    let none = |_: &Ending<Memory>| {};
    let with_one = |s: &Ending<Memory>| block_on(s.put("job:1", "a".into())).unwrap();
    assert_eq!(failing_each_step(none, |s| block_on(s.put("job:1", "a".into()))), 2);
    assert_eq!(failing_each_step(none, |s| block_on(s.put_if_absent("job:1", "a".into()))), 2);
    assert_eq!(failing_each_step(with_one, |s| block_on(s.put_if_absent("job:1", "b".into()))), 1);
    assert_eq!(failing_each_step(with_one, |s| block_on(s.delete("job:1"))), 2);
    assert_eq!(failing_each_step(none, |s| block_on(s.put_until("job:1", "a".into(), 5))), 3);
    assert_eq!(failing_each_step(with_one, |s| block_on(s.swap_until("job:1", Some("a"), "b".into(), Some(5)))), 4);
    assert_eq!(failing_each_step(with_one, |s| block_on(s.swap("job:1", Some("a"), "b".into()))), 3);
    assert_eq!(failing_each_step(with_one, |s| block_on(s.swap("job:1", Some("x"), "b".into()))), 1);
    // Its expiry: a record due, one written again since its mark (it keeps its newer end), one not due yet, and marks
    // that are not its own (left as they are).
    let kept = |s: &Ending<Memory>| {
        block_on(s.put_until("job:due", "a".into(), 5)).unwrap();
        block_on(s.put_until("job:again", "b".into(), 6)).unwrap();
        block_on(s.put_until("job:again", "c".into(), 50)).unwrap();
        block_on(s.put_until("job:later", "d".into(), 500)).unwrap();
        block_on(s.0.put("~x:nocolon", String::new())).unwrap();
        block_on(s.0.put("~x:soon:job:odd", String::new())).unwrap();
    };
    assert_eq!(failing_each_step(kept, |s| block_on(s.expire(100))), 11);
    let store = Ending(Memory::default());
    kept(&store);
    assert_eq!(block_on(store.expire(100)), Ok(2));
    assert_eq!(store.0.keys(), ["job:later", "~e:job:later", "~x:0000000000500:job:later", "~x:nocolon", "~x:soon:job:odd"]);
    // Nothing given an end: nothing to do.
    assert_eq!(block_on(Ending(Memory::default()).expire(100)), Ok(0));
}

#[test]
fn a_machine_is_told_its_work_as_json() {
    assert_eq!(Work::GitHub { jit: "j".into() }.json(), serde_json::json!({ "jit": "j" }));
    assert_eq!(Work::GitLab { url: "https://gitlab.com".into(), token: "t".into() }.json(), serde_json::json!({ "gitlab": { "url": "https://gitlab.com", "token": "t" } }));
    assert_eq!(Work::Fail { jit: "j".into(), why: "w".into() }.json(), serde_json::json!({ "jit": "j", "fail": "w" }));
}

/// A moment is before another only strictly: what lasts until a moment ends at it. An answer is done only with a
/// 2xx status: not a redirect, nor anything under 200.
#[test]
fn a_moment_is_before_another_only_strictly_and_an_answer_is_done_only_with_a_2xx_status() {
    use superci_core::io::{before, done};
    assert!(before(9, 10) && !before(10, 10) && !before(11, 10));
    assert!(done(200) && done(204) && done(299) && !done(300) && !done(199) && !done(404) && !done(0));
}

/// The start of a month, around each leap rule: a year a hundred divides is no leap year, one four hundred divides is
/// (2000, 2400), and the days of each century of the 400-year cycle (checked against Python's calendar).
#[test]
fn a_months_start_holds_across_every_leap_rule_of_the_calendar() {
    for (at, start) in [
        (951825600000u64, 949363200000u64), // 2000-02-29 12:00
        (951868800000, 951868800000), // 2000-03-01 00:00
        (3981312000000, 3978892800000), // 2096-02-29 00:00
        (4012934400000, 4012934400000), // 2097-03-01 00:00
        (4102444740000, 4099766400000), // 2099-12-31 23:59
        (4102444800000, 4102444800000), // 2100-01-01 00:00
        (4107542340000, 4105123200000), // 2100-02-28 23:59
        (4107542400000, 4107542400000), // 2100-03-01 00:00
        (7258075200000, 7255440000000), // 2199-12-31 12:00
        (7263216000000, 7263216000000), // 2200-03-01 00:00
        (10387267200000, 10384934400000), // 2299-02-28 00:00
        (10421567940000, 10418889600000), // 2300-03-31 23:59
        (13569465540000, 13566787200000), // 2399-12-31 23:59
        (13574563200000, 13572144000000), // 2400-02-29 00:00
        (13574649600000, 13574649600000), // 2400-03-01 00:00
        (13601088000000, 13601088000000), // 2401-01-01 00:00
    ] {
        assert_eq!(superci_core::plane::month_start_ms(at), start, "{at}");
    }
}

/// An account nothing was spent in comes to nothing, not to minus nothing (an empty sum of floats is -0.0, which a
/// page would show as "$-0").
#[test]
fn an_account_with_no_spend_comes_to_a_plain_zero() {
    let spent = superci_core::plane::Records::default().account_spend([("modal", 2.5)], "aws");
    assert!(spent == 0.0 && spent.is_sign_positive(), "{spent}");
    assert_eq!(superci_core::plane::dollars(spent), "$0");
    assert_eq!(superci_core::plane::Records::default().account_spend([("aws", 1.5), ("aws-on-demand", 1.0), ("modal", 4.0)], "aws"), 2.5);
}

/// A day's calendar date, around each leap rule (checked against Python's calendar).
#[test]
fn a_days_date_holds_across_every_leap_rule_of_the_calendar() {
    use superci_core::plane::civil;
    for (days, date) in [(0, (1970, 1, 1)), (11016, (2000, 2, 29)), (11017, (2000, 3, 1)), (47540, (2100, 2, 28)), (47541, (2100, 3, 1)), (84005, (2199, 12, 31)), (84065, (2200, 3, 1)), (157112, (2400, 2, 28)), (157113, (2400, 2, 29)), (157114, (2400, 3, 1)), (157419, (2400, 12, 31)), (-1, (1969, 12, 31))] {
        assert_eq!(civil(days), date, "{days}");
    }
}
