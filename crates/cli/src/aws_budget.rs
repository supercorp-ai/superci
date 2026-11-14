//! A budget in the AWS account itself, beside the account's monthly limit in SuperCI: AWS counts the account's real
//! bill (hours late) and emails at 80% and at 100% of the limit; asked to, it also puts a rule on the control plane's
//! role at 100% that refuses starting machines, and takes it off when the next month begins.
//!
//! All of it is made, changed and removed here with the sign-in of the person at the dashboard or a command. The
//! control plane has no part in it and gets no permission for it. What is made is named by the control plane it is
//! for (`aws::budget_name`, `aws::stop_policy_name`, `aws::budget_role_name`), so it is found again by name and
//! removing it can be repeated. A budget the account's owner made is never touched.
use serde_json::{json, Value};

use superci_core::aws::{self, Credentials};
use superci_core::io::Http;

use crate::Result;

/// The budget as it is in the account now.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Found {
    /// Its limit for the month, and what AWS has counted of the month so far (before credits).
    pub amount: f64,
    pub spent: f64,
    /// Whether it has the action that stops new machines, and whether that rule is on the role now.
    pub stop: bool,
    pub stopped: bool,
}

pub struct Budgets<'a> {
    pub http: &'a dyn Http,
    pub creds: &'a Credentials,
    pub account: &'a str,
    pub plane_id: &'a str,
    /// Waits so many milliseconds (a role just made is not known everywhere in AWS for some seconds).
    pub pause: &'a dyn Fn(u64),
}

/// The budget of a control plane's AWS account, asked of AWS itself with a sign-in to that account.
pub fn with<R>(creds: &Credentials, account: &str, plane_id: &str, f: impl FnOnce(&Budgets) -> R) -> R {
    let http = crate::aws::Blocking::new();
    f(&Budgets { http: &http, creds, account, plane_id, pause: &crate::aws::pause })
}

fn now_ms() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0) }

/// A dollar amount as AWS Budgets takes it.
fn amount_text(usd: f64) -> String { if usd.fract() == 0.0 { format!("{usd:.0}") } else { format!("{usd:.2}") } }

/// The rule AWS puts on the control plane's role at the limit: no new machine, by any way of asking for one. But
/// for the one small kind that takes a refused job at GitHub and fails it with the reason (`aws::SAYS_WHY_TYPES`):
/// without it a refused job would wait at GitHub for a runner that never comes, saying nothing.
pub fn stop_policy() -> Value {
    json!({ "Version": "2012-10-17", "Statement": [
        { "Sid": "NoNewMachinesButTheOneThatSaysWhy", "Effect": "Deny", "Action": "ec2:RunInstances", "Resource": "arn:aws:ec2:*:*:instance/*", "Condition": { "StringNotEquals": { "ec2:InstanceType": aws::SAYS_WHY_TYPES } } },
        { "Sid": "NoOtherWayToStartOne", "Effect": "Deny", "Action": ["ec2:CreateFleet", "ec2:RequestSpotInstances", "ec2:RequestSpotFleet"], "Resource": "*" }] })
}

impl Budgets<'_> {
    fn name(&self) -> String { aws::budget_name(self.plane_id) }
    fn policy_arn(&self) -> String { format!("arn:aws:iam::{}:policy/{}", self.account, aws::stop_policy_name(self.plane_id)) }
    fn role_arn(&self) -> String { format!("arn:aws:iam::{}:role/{}", self.account, aws::budget_role_name(self.plane_id)) }
    /// The role the control plane starts machines with (see `superci_core::plane::role_arn`).
    fn plane_role(&self) -> String { format!("superci-plane-{}", self.plane_id) }

    fn budgets(&self, target: &str, mut body: Value) -> Result<Value> {
        body["AccountId"] = self.account.into();
        futures::executor::block_on(aws::json_call(self.http, "POST", "https://budgets.amazonaws.com/", "us-east-1", "budgets", Some(&format!("AWSBudgetServiceGateway.{target}")), "application/x-amz-json-1.1", Some(&body), self.creds, now_ms()))
    }
    fn iam(&self, action: &str, params: Value) -> Result<String> { futures::executor::block_on(aws::iam(self.http, self.creds, action, params, now_ms())) }

    /// The budget's definition: the month's real costs of the whole account, before credits and refunds (a bill that
    /// credits pay is still spending).
    fn budget(&self, usd: f64) -> Value {
        json!({ "BudgetName": self.name(), "BudgetLimit": { "Amount": amount_text(usd), "Unit": "USD" }, "TimeUnit": "MONTHLY", "BudgetType": "COST",
            "CostTypes": { "IncludeCredit": false, "IncludeRefund": false, "IncludeTax": true, "IncludeSubscription": true, "IncludeUpfront": true, "IncludeRecurring": true,
                "IncludeOtherSubscription": true, "IncludeSupport": true, "IncludeDiscount": true, "UseBlended": false, "UseAmortized": false } })
    }

    /// The budget's actions: (id, status).
    fn actions(&self) -> Result<Vec<(String, String)>> {
        let v = match self.budgets("DescribeBudgetActionsForBudget", json!({ "BudgetName": self.name() })) { Ok(v) => v, Err(e) if e.starts_with("NotFoundException") => return Ok(vec![]), Err(e) => return Err(e) };
        Ok(v["Actions"].as_array().into_iter().flatten().filter_map(|a| Some((a["ActionId"].as_str()?.to_string(), a["Status"].as_str().unwrap_or_default().to_string()))).collect())
    }

    /// What is there now (None: no budget of SuperCI's for this control plane).
    pub fn found(&self) -> Result<Option<Found>> {
        let v = match self.budgets("DescribeBudget", json!({ "BudgetName": self.name() })) { Ok(v) => v, Err(e) if e.starts_with("NotFoundException") => return Ok(None), Err(e) => return Err(e) };
        let number = |v: &Value| v["Amount"].as_str().and_then(|a| a.parse::<f64>().ok()).unwrap_or(0.0);
        let actions = self.actions()?;
        Ok(Some(Found { amount: number(&v["Budget"]["BudgetLimit"]), spent: number(&v["Budget"]["CalculatedSpend"]["ActualSpend"]),
            stop: !actions.is_empty(), stopped: actions.iter().any(|(_, status)| status == "EXECUTION_SUCCESS") }))
    }

    /// The first address the budget's emails go to.
    fn email(&self) -> Result<Option<String>> {
        let v = self.budgets("DescribeNotificationsForBudget", json!({ "BudgetName": self.name() }))?;
        for n in v["Notifications"].as_array().into_iter().flatten() {
            let s = self.budgets("DescribeSubscribersForNotification", json!({ "BudgetName": self.name(), "Notification": n }))?;
            if let Some(a) = s["Subscribers"].as_array().into_iter().flatten().find(|s| s["SubscriptionType"] == "EMAIL").and_then(|s| s["Address"].as_str()) { return Ok(Some(a.to_string())) }
        }
        Ok(None)
    }

    /// Makes the budget what is asked: `usd` a month, emails to `email` (needed when the budget is first made; left
    /// out later, the address it has stays), and with `stop` the action that refuses new machines at 100%. A limit
    /// raised after the action ran takes the rule off the role and has AWS judge it anew.
    pub fn set(&self, usd: f64, email: Option<&str>, stop: bool) -> Result<()> {
        let before = self.found()?;
        match &before {
            None => {
                let to = email.filter(|e| !e.is_empty()).ok_or("An address for AWS's emails is needed: --email=you@example.com (AWS writes at 80% and at 100% of the limit).")?;
                let at = |percent: u32| json!({ "Notification": { "NotificationType": "ACTUAL", "ComparisonOperator": "GREATER_THAN", "Threshold": percent, "ThresholdType": "PERCENTAGE" },
                    "Subscribers": [{ "SubscriptionType": "EMAIL", "Address": to }] });
                self.budgets("CreateBudget", json!({ "Budget": self.budget(usd), "NotificationsWithSubscribers": [at(80), at(100)] }))?;
            }
            Some(b) if b.amount != usd => { self.budgets("UpdateBudget", json!({ "NewBudget": self.budget(usd) }))?; }
            Some(_) => {}
        }
        let had = before.as_ref().is_some_and(|b| b.stop);
        if stop && !had { self.add_stop(email)? }
        if !stop && had { self.remove_stop()? }
        // Raised after it ran: the rule comes off, and AWS judges the new limit from here on.
        if stop && before.as_ref().is_some_and(|b| b.stopped && usd > b.amount) { self.lift(true)? }
        Ok(())
    }

    /// The rule, the role AWS Budgets puts it on with, and the action that does so at 100% of the budget.
    fn add_stop(&self, email: Option<&str>) -> Result<()> {
        let to = match email.filter(|e| !e.is_empty()) { Some(e) => e.to_string(), None => self.email()?.ok_or("An address for AWS's emails is needed: --email=you@example.com.")? };
        let exists = |r: Result<String>| match r { Err(e) if e.starts_with("EntityAlreadyExists") => Ok(()), Err(e) => Err(e), Ok(_) => Ok(()) };
        exists(self.iam("CreatePolicy", json!({ "PolicyName": aws::stop_policy_name(self.plane_id), "PolicyDocument": stop_policy().to_string(),
            "Description": "No new machines: put on a SuperCI control plane's role by the account's budget at its limit, and taken off when the next month begins." })))?;
        // Only AWS Budgets, for this account's budget, may use the role; it may put that one rule on that one role.
        let trust = json!({ "Version": "2012-10-17", "Statement": [{ "Effect": "Allow", "Principal": { "Service": "budgets.amazonaws.com" }, "Action": "sts:AssumeRole",
            "Condition": { "StringEquals": { "aws:SourceAccount": self.account }, "ArnLike": { "aws:SourceArn": format!("arn:aws:budgets::{}:budget/{}", self.account, self.name()) } } }] });
        exists(self.iam("CreateRole", json!({ "RoleName": aws::budget_role_name(self.plane_id), "AssumeRolePolicyDocument": trust.to_string(),
            "Description": "AWS Budgets puts the no-new-machines rule on a SuperCI control plane's role with it, at the budget's limit.",
            "Tags": aws::plane_tags_iam(self.plane_id) })))?;
        let may = json!({ "Version": "2012-10-17", "Statement": [{ "Effect": "Allow", "Action": ["iam:AttachRolePolicy", "iam:DetachRolePolicy"],
            "Resource": format!("arn:aws:iam::{}:role/{}", self.account, self.plane_role()), "Condition": { "ArnEquals": { "iam:PolicyARN": self.policy_arn() } } }] });
        self.iam("PutRolePolicy", json!({ "RoleName": aws::budget_role_name(self.plane_id), "PolicyName": "put-the-rule", "PolicyDocument": may.to_string() }))?;
        let action = json!({ "BudgetName": self.name(), "NotificationType": "ACTUAL", "ActionType": "APPLY_IAM_POLICY", "ApprovalModel": "AUTOMATIC",
            "ActionThreshold": { "ActionThresholdValue": 100, "ActionThresholdType": "PERCENTAGE" },
            "Definition": { "IamActionDefinition": { "PolicyArn": self.policy_arn(), "Roles": [self.plane_role()] } },
            "ExecutionRoleArn": self.role_arn(), "Subscribers": [{ "SubscriptionType": "EMAIL", "Address": to }] });
        // (A role just made is not known to every part of AWS for some seconds: asked again while it says so.)
        let mut tries = 0;
        loop {
            match self.budgets("CreateBudgetAction", action.clone()) {
                Ok(_) => return Ok(()),
                Err(e) if tries < 8 && (e.contains("assume") || e.contains("AccessDenied") || e.contains("InvalidParameter")) => { tries += 1; (self.pause)(3000) }
                Err(e) => return Err(e),
            }
        }
    }

    /// Takes the rule off the control plane's role, if AWS put it there. `again`: AWS judges the budget anew in this
    /// same month (after its limit was raised); without it the rule stays off until the next month.
    pub fn lift(&self, again: bool) -> Result<()> {
        for (id, status) in self.actions()? {
            if status == "EXECUTION_SUCCESS" {
                self.budgets("ExecuteBudgetAction", json!({ "BudgetName": self.name(), "ActionId": id, "ExecutionType": "REVERSE_BUDGET_ACTION" }))?;
                self.settled(&id)?;
            }
            if again && ["EXECUTION_SUCCESS", "REVERSE_SUCCESS"].contains(&status.as_str()) {
                self.budgets("ExecuteBudgetAction", json!({ "BudgetName": self.name(), "ActionId": id, "ExecutionType": "RESET_BUDGET_ACTION" }))?;
            }
        }
        // (And directly, should AWS not have: the person signed in may take a rule off a role of their account.)
        self.off_the_role()
    }

    /// Waits for an action AWS is running or undoing to be done with it.
    fn settled(&self, id: &str) -> Result<()> {
        // (Thirty seconds at most: then on, as if it were.)
        let mut asked = 0;
        while self.actions()?.iter().any(|(a, status)| a == id && status.ends_with("_IN_PROGRESS")) {
            (self.pause)(1500);
            asked += 1;
            if asked == 20 { break }
        }
        Ok(())
    }

    fn off_the_role(&self) -> Result<()> {
        match self.iam("DetachRolePolicy", json!({ "RoleName": self.plane_role(), "PolicyArn": self.policy_arn() })) { Err(e) if !e.starts_with("NoSuchEntity") => Err(e), _ => Ok(()) }
    }

    /// Removes the action, its role and the rule (the rule is taken off the control plane's role first). The budget
    /// and its emails stay.
    fn remove_stop(&self) -> Result<()> {
        let gone = |r: Result<String>| match r { Err(e) if e.starts_with("NoSuchEntity") => Ok(()), Err(e) => Err(e), Ok(_) => Ok(()) };
        for (id, status) in self.actions()? {
            if status == "EXECUTION_SUCCESS" {
                self.budgets("ExecuteBudgetAction", json!({ "BudgetName": self.name(), "ActionId": id, "ExecutionType": "REVERSE_BUDGET_ACTION" }))?;
            }
            self.settled(&id)?;
            match self.budgets("DeleteBudgetAction", json!({ "BudgetName": self.name(), "ActionId": id })) { Err(e) if !e.starts_with("NotFoundException") => return Err(e), _ => {} }
        }
        self.off_the_role()?;
        gone(self.iam("DeleteRolePolicy", json!({ "RoleName": aws::budget_role_name(self.plane_id), "PolicyName": "put-the-rule" })))?;
        gone(self.iam("DeleteRole", json!({ "RoleName": aws::budget_role_name(self.plane_id) })))?;
        gone(self.iam("DeletePolicy", json!({ "PolicyArn": self.policy_arn() })))
    }

    /// Removes everything made here for this control plane: the action with its role and rule, then the budget.
    /// Safe to repeat, and when nothing was ever made.
    pub fn remove(&self) -> Result<()> {
        self.remove_stop()?;
        match self.budgets("DeleteBudget", json!({ "BudgetName": self.name() })) { Err(e) if !e.starts_with("NotFoundException") => Err(e), _ => Ok(()) }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::aws::replay::Replay;

    /// What a real account answered to each step, in the order asked (a project of AWS's new sign-up with its
    /// advanced features on, 13 October 2026): no budget; one made with emails; its limit raised; the action that
    /// stops machines added (AWS refused the new role three times before it knew it); the rule taken off the role;
    /// everything removed; and removing once more with nothing there.
    pub const RECORDED: &str = concat!(include_str!("../fixtures/aws/budget/01-none.jsonl"), include_str!("../fixtures/aws/budget/02-emails.jsonl"), include_str!("../fixtures/aws/budget/03-emails-found.jsonl"),
        include_str!("../fixtures/aws/budget/04-raise.jsonl"), include_str!("../fixtures/aws/budget/05-raise-found.jsonl"), include_str!("../fixtures/aws/budget/06-stop.jsonl"),
        include_str!("../fixtures/aws/budget/07-stop-found.jsonl"), include_str!("../fixtures/aws/budget/08-lift.jsonl"), include_str!("../fixtures/aws/budget/09-remove.jsonl"),
        include_str!("../fixtures/aws/budget/10-removed-found.jsonl"), include_str!("../fixtures/aws/budget/11-remove-again.jsonl"));

    #[test]
    fn the_accounts_own_budget_is_made_changed_and_removed_as_aws_answered() {
        let aws = Replay::in_order(RECORDED);
        let creds = Credentials { access_key_id: "AKIDEXAMPLE".into(), secret_access_key: "secret".into(), session_token: Some("token".into()), expires_at_ms: 0 };
        let waited = std::cell::Cell::new(0u64);
        let b = Budgets { http: &aws, creds: &creds, account: "111111111111", plane_id: "abcdefghijkl", pause: &|ms| waited.set(waited.get() + ms) };
        // Nothing yet; making one needs where AWS writes to.
        assert_eq!(b.found().unwrap(), None);
        let aws_unasked = Replay::in_order(include_str!("../fixtures/aws/budget/01-none.jsonl"));
        let unasked = Budgets { http: &aws_unasked, creds: &creds, account: "111111111111", plane_id: "abcdefghijkl", pause: &|_| {} };
        assert!(unasked.set(100.0, None, false).unwrap_err().starts_with("An address for AWS's emails is needed"));
        // A budget with emails at 80% and 100%; then its limit raised (the emails stay as they are).
        b.set(100.0, Some("someone@example.com"), false).unwrap();
        assert_eq!(b.found().unwrap(), Some(Found { amount: 100.0, spent: 0.0, stop: false, stopped: false }));
        b.set(120.0, None, false).unwrap();
        assert_eq!(b.found().unwrap(), Some(Found { amount: 120.0, spent: 0.0, stop: false, stopped: false }));
        // The stop: the rule, the role AWS Budgets uses, and the action, written to at the budget's own address.
        // AWS did not know the new role for some seconds: asked again each time, and then it took it.
        b.set(120.0, None, true).unwrap();
        assert_eq!(waited.get(), 9000);
        assert_eq!(b.found().unwrap(), Some(Found { amount: 120.0, spent: 0.0, stop: true, stopped: false }));
        // The rule taken off the control plane's role (it was put there by hand for the recording).
        b.lift(false).unwrap();
        // All of it removed; and once more, with nothing there, is no error.
        b.remove().unwrap();
        assert_eq!(b.found().unwrap(), None);
        b.remove().unwrap();
        assert_eq!(aws.left(), 0, "every recorded answer was asked for, in its order");
    }

    #[test]
    fn the_rule_refuses_every_way_of_starting_a_machine_but_the_small_one_that_says_why() {
        let p = stop_policy();
        let statements = p["Statement"].as_array().unwrap();
        assert!(statements.len() == 2 && statements.iter().all(|s| s["Effect"] == "Deny"));
        // A machine of any kind but the two small ones is refused; those are what the runner that fails a refused
        // job at GitHub asks for first, and no job's own machine is ever one.
        assert_eq!((&statements[0]["Action"], &statements[0]["Condition"]), (&json!("ec2:RunInstances"), &json!({ "StringNotEquals": { "ec2:InstanceType": ["t3a.small", "t3.small"] } })));
        assert_eq!((&statements[1]["Action"], &statements[1]["Resource"]), (&json!(["ec2:CreateFleet", "ec2:RequestSpotInstances", "ec2:RequestSpotFleet"]), &json!("*")));
    }

    /// An account as a test has it: a budget of $100 (or none), these actions and notifications, and the `n`-th call
    /// of `failing` (its target or action, with an action's execution type) answered with AWS's internal error.
    struct Account { budget: bool, actions: Value, notifications: Value, failing: (&'static str, usize), asked: std::sync::Mutex<Vec<String>> }

    #[async_trait::async_trait(?Send)]
    impl Http for Account {
        async fn send(&self, r: superci_core::io::Request) -> superci_core::io::Result<superci_core::io::Response> {
            let body: Value = serde_json::from_slice(&r.body).unwrap_or_default();
            let target = r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("x-amz-target")).map(|(_, v)| v.trim_start_matches("AWSBudgetServiceGateway.").to_string());
            let what = match target {
                Some(t) => format!("{t} {}", body["ExecutionType"].as_str().unwrap_or_default()).trim_end().to_string(),
                None => String::from_utf8_lossy(&r.body).split('&').find_map(|kv| kv.strip_prefix("Action=")).unwrap_or_default().to_string(),
            };
            let mut asked = self.asked.lock().unwrap();
            asked.push(what.clone());
            let iam = r.url.contains("iam.");
            let (status, answer) = if what.starts_with(self.failing.0) && asked.iter().filter(|a| a.starts_with(self.failing.0)).count() == self.failing.1 {
                if iam { (500, "<ErrorResponse><Error><Code>ServiceFailure</Code><Message>Try again.</Message></Error></ErrorResponse>".to_string()) } else { (500, json!({ "__type": "InternalErrorException", "Message": "Try again." }).to_string()) }
            } else {
                match what.as_str() {
                    "DescribeBudget" if !self.budget => (400, json!({ "__type": "NotFoundException", "Message": "No budget" }).to_string()),
                    "DescribeBudget" => (200, json!({ "Budget": { "BudgetLimit": { "Amount": "100" }, "CalculatedSpend": { "ActualSpend": { "Amount": "5" } } } }).to_string()),
                    "DescribeBudgetActionsForBudget" => (200, json!({ "Actions": self.actions }).to_string()),
                    "DescribeNotificationsForBudget" => (200, json!({ "Notifications": self.notifications }).to_string()),
                    "DescribeSubscribersForNotification" => (200, json!({ "Subscribers": [{ "SubscriptionType": "EMAIL", "Address": "someone@example.com" }] }).to_string()),
                    _ if iam => (200, "<Ok/>".to_string()),
                    _ => (200, "{}".to_string()),
                }
            };
            Ok(superci_core::io::Response { status, headers: vec![], body: answer.into_bytes() })
        }
    }

    fn account(budget: bool, actions: Value, failing: (&'static str, usize)) -> Account {
        Account { budget, actions, notifications: json!([{ "NotificationType": "ACTUAL" }]), failing, asked: Default::default() }
    }

    /// `f` with the budget of this account.
    fn budgets<R>(aws: &Account, f: impl FnOnce(&Budgets) -> R) -> R {
        let creds = Credentials { access_key_id: "AKIDEXAMPLE".into(), secret_access_key: "secret".into(), session_token: None, expires_at_ms: 0 };
        f(&Budgets { http: aws, creds: &creds, account: "111111111111", plane_id: "abcdefghijkl", pause: &|_| {} })
    }

    const SUCCEEDED: &str = r#"[{ "ActionId": "a1", "Status": "EXECUTION_SUCCESS" }]"#;

    fn actions(json: &str) -> Value { serde_json::from_str(json).unwrap() }

    #[test]
    fn a_budget_whose_actions_cannot_be_read_is_not_said_found_and_an_action_without_its_id_is_none() {
        assert!(budgets(&account(true, json!([]), ("DescribeBudgetActionsForBudget", 1)), |b| b.found()).is_err());
        assert_eq!(budgets(&account(true, json!([{ "Status": "STANDBY" }]), ("", 0)), |b| b.found()).unwrap(), Some(Found { amount: 100.0, spent: 5.0, stop: false, stopped: false }));
    }

    #[test]
    fn a_budget_aws_does_not_make_or_change_is_an_error() {
        assert!(budgets(&account(false, json!([]), ("CreateBudget", 1)), |b| b.set(100.0, Some("someone@example.com"), false)).unwrap_err().starts_with("InternalErrorException"));
        assert!(budgets(&account(true, json!([]), ("UpdateBudget", 1)), |b| b.set(120.0, None, false)).unwrap_err().starts_with("InternalErrorException"));
    }

    #[test]
    fn the_stop_is_taken_off_when_no_longer_asked_for() {
        let aws = account(true, actions(SUCCEEDED), ("", 0));
        budgets(&aws, |b| b.set(100.0, None, false)).unwrap();
        assert!(aws.asked.lock().unwrap().iter().any(|a| a == "DeleteBudgetAction"));
    }

    /// The limit raised after the stop ran: the rule must come off, and AWS not taking it off is an error.
    #[test]
    fn a_limit_raised_after_the_stop_ran_fails_when_the_rule_does_not_come_off() {
        assert!(budgets(&account(true, actions(SUCCEEDED), ("ExecuteBudgetAction REVERSE", 1)), |b| b.set(120.0, None, true)).is_err());
    }

    /// The stop added with no address given: the budget's own, from its notifications; none there, or not read, and
    /// the stop is not added.
    #[test]
    fn a_stop_added_without_an_address_needs_the_budgets_own() {
        let set = |aws: &Account| budgets(aws, |b| b.set(100.0, None, true));
        for failing in ["DescribeNotificationsForBudget", "DescribeSubscribersForNotification"] { assert!(set(&account(true, json!([]), (failing, 1))).is_err(), "{failing}") }
        let mut none = account(true, json!([]), ("", 0));
        none.notifications = json!([]);
        assert!(set(&none).unwrap_err().starts_with("An address for AWS's emails is needed"));
        set(&account(true, json!([]), ("", 0))).unwrap();
        for failing in ["CreatePolicy", "PutRolePolicy"] { assert!(budgets(&account(true, json!([]), (failing, 1)), |b| b.set(100.0, Some("someone@example.com"), true)).is_err(), "{failing}") }
    }

    /// Lifting the rule: with no action there is only the role to take it off; each of AWS's steps failing is an
    /// error (undoing the action, waiting for it, judging the budget anew).
    #[test]
    fn lifting_the_rule_fails_at_whichever_step_aws_refuses() {
        budgets(&account(true, json!([]), ("", 0)), |b| b.lift(true)).unwrap();
        for failing in [("DescribeBudgetActionsForBudget", 1), ("ExecuteBudgetAction REVERSE", 1), ("DescribeBudgetActionsForBudget", 2)] { assert!(budgets(&account(true, actions(SUCCEEDED), failing), |b| b.lift(false)).is_err(), "{failing:?}") }
        assert!(budgets(&account(true, actions(r#"[{ "ActionId": "a1", "Status": "REVERSE_SUCCESS" }]"#), ("ExecuteBudgetAction RESET", 1)), |b| b.lift(true)).is_err());
    }

    /// Removing: an action AWS is still undoing is waited for half a minute, then removed all the same; each of
    /// AWS's steps failing is an error.
    #[test]
    fn removing_waits_for_an_action_being_undone_and_fails_at_whichever_step_aws_refuses() {
        let aws = account(true, actions(r#"[{ "ActionId": "a1", "Status": "REVERSE_IN_PROGRESS" }]"#), ("", 0));
        budgets(&aws, |b| b.remove()).unwrap();
        assert_eq!(aws.asked.lock().unwrap().iter().filter(|a| *a == "DescribeBudgetActionsForBudget").count(), 21);
        for failing in [("DescribeBudgetActionsForBudget", 1), ("ExecuteBudgetAction REVERSE", 1), ("DescribeBudgetActionsForBudget", 2), ("DeleteRole", 2)] { assert!(budgets(&account(true, actions(SUCCEEDED), failing), |b| b.remove()).is_err(), "{failing:?}") }
    }
}
