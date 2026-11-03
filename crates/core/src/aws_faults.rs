//! AWS's calls with each of them failing in turn, as `superci_testkit::sweep` does to whole scenarios: dropped, answered
//! with AWS's error of each kind that SuperCI tells apart, or answered with nothing in it. Whatever fails, nothing
//! panics, and an error says what failed; the kinds SuperCI takes in its stride (a route already there, a group
//! already gone) are taken.
use super::*;
use std::cell::{Cell, RefCell};

/// What AWS answers to a call (`what`: its action, its target, or its method and address), as a test has it.
type Answer = Box<dyn Fn(&str, &Request) -> (u16, String)>;

/// How the call that fails fails.
#[derive(Clone, Copy, Debug)]
enum Fails { Dropped, Empty, Code(&'static str) }

/// Each kind: the errors SuperCI tells apart, and AWS's own fault.
const KINDS: [Fails; 12] = [Fails::Dropped, Fails::Empty, Fails::Code("InternalError"), Fails::Code("Resource.AlreadyAssociated"), Fails::Code("RouteAlreadyExists"),
    Fails::Code("InvalidGroup.NotFound"), Fails::Code("RequestLimitExceeded"), Fails::Code("InsufficientInstanceCapacity"), Fails::Code("DryRunOperation"),
    Fails::Code("NoSuchResourceException"), Fails::Code("BucketAlreadyOwnedByYou"), Fails::Code("UnauthorizedOperation")];

/// One call as one line: the Query API's action, the JSON API's target, else its method and address.
fn what(r: &Request) -> String {
    let action = url::form_urlencoded::parse(&r.body).find(|(k, _)| k == "Action").map(|(_, v)| v.into_owned());
    action.or_else(|| r.header("x-amz-target").map(str::to_string)).unwrap_or_else(|| format!("{} {}", r.method, r.url))
}

/// AWS answering as `answer` says, the k-th call (from 1) failing as said; each call counted.
struct Aws { answer: Answer, fail: Option<(usize, Fails)>, n: Cell<usize>, calls: RefCell<Vec<String>> }

#[async_trait::async_trait(?Send)]
impl Http for Aws {
    async fn send(&self, r: Request) -> Result<crate::io::Response> {
        let w = what(&r);
        self.calls.borrow_mut().push(w.clone());
        self.n.set(self.n.get() + 1);
        let failing = self.fail.filter(|(k, _)| *k == self.n.get()).map(|(_, f)| f);
        let json = r.header("x-amz-target").is_some() || r.url.contains("lambda.");
        let (status, body) = match failing {
            None => (self.answer)(&w, &r),
            Some(Fails::Dropped) => return Err(format!("{w}: the connection was dropped")),
            Some(Fails::Empty) => (200, String::new()),
            Some(Fails::Code(code)) if json => (400, serde_json::json!({ "__type": code, "message": format!("{code} said") }).to_string()),
            Some(Fails::Code(code)) if r.url.contains(".s3.") => (if code == "InternalError" { 500 } else { 409 }, format!("<Error><Code>{code}</Code><Message>{code} said</Message></Error>")),
            Some(Fails::Code(code)) => (if code == "InternalError" { 500 } else { 400 }, format!("<Response><Errors><Error><Code>{code}</Code><Message>{code} said</Message></Error></Errors></Response>")),
        };
        Ok(crate::io::Response::new(status, if json { "application/x-amz-json-1.1" } else { "text/xml" }, body))
    }
}

fn creds() -> Credentials { Credentials { access_key_id: "AKIDEXAMPLE".into(), secret_access_key: "secret".into(), session_token: Some("token".into()), expires_at_ms: u64::MAX } }

/// `run` with AWS answering as `fresh` makes it, clean (it must work: what it gives is returned), then again for each
/// call it made and each kind of failing, that call failing so: held to no panic and an error that says something.
fn swept<T: std::fmt::Debug>(fresh: impl Fn() -> Answer, run: impl Fn(&dyn Http) -> Result<T>) -> T {
    let clean = Aws { answer: fresh(), fail: None, n: Cell::new(0), calls: RefCell::default() };
    let got = run(&clean).unwrap_or_else(|e| panic!("clean: {e} (calls: {:?})", clean.calls.borrow()));
    let calls = clean.calls.borrow().clone();
    for k in 1..=calls.len() {
        for kind in KINDS {
            let aws = Aws { answer: fresh(), fail: Some((k, kind)), n: Cell::new(0), calls: RefCell::default() };
            if let Err(e) = run(&aws) { assert!(!e.trim().is_empty(), "call {k} ({}) failing as {kind:?}: an error that says nothing", calls[k - 1]) }
        }
    }
    got
}

/// `run` once, nothing failing (where a sweep would ask thousands of calls for little more).
fn once<T>(answer: Answer, run: impl Fn(&dyn Http) -> Result<T>) -> T {
    run(&Aws { answer, fail: None, n: Cell::new(0), calls: RefCell::default() }).unwrap_or_else(|e| panic!("{e}"))
}

fn ok() -> (u16, String) { (200, "<return>true</return>".into()) }

/// EC2 holding no network yet, answering what making one asks; `zones` of them in the region, and `first` the VPC
/// another making at once made first (none: none).
fn empty_region(zones: usize, first: Option<&'static str>) -> Answer {
    let made = Cell::new(false);
    Box::new(move |w, _| match w {
        "DescribeVpcs" if !made.get() => (200, String::new()),
        "DescribeVpcs" => (200, format!("<vpcId>vpc-1</vpcId>{}", first.map(|f| format!("<vpcId>{f}</vpcId>")).unwrap_or_default())),
        "CreateVpc" => { made.set(true); (200, "<vpcId>vpc-1</vpcId>".into()) }
        "CreateInternetGateway" => (200, "<internetGatewayId>igw-1</internetGatewayId>".into()),
        "CreateRouteTable" => (200, "<routeTableId>rtb-1</routeTableId>".into()),
        "DescribeAvailabilityZones" => (200, (0..zones).map(|i| format!("<zoneName>us-east-1{}</zoneName>", (b'a' + i as u8) as char)).collect()),
        "CreateSubnet" => (200, "<subnetId>subnet-1</subnetId>".into()),
        "CreateSecurityGroup" => (200, "<groupId>sg-1</groupId>".into()),
        "DescribeInternetGateways" | "DescribeRouteTables" | "DescribeSubnets" | "DescribeVpcEndpoints" | "DescribeSecurityGroups" => (200, String::new()),
        _ => ok(),
    })
}

/// EC2 holding a network whole: a VPC of `whose`'s tag, its gateway attached, its route table, a subnet in each zone,
/// its endpoint and its group.
fn whole_region(own: bool) -> Answer {
    Box::new(move |w, r| {
        let asked = String::from_utf8_lossy(&r.body).to_string();
        match w {
            "DescribeVpcs" if own == asked.contains("superci-plane") => (200, "<vpcId>vpc-9</vpcId>".into()),
            "DescribeVpcs" => (200, String::new()),
            "DescribeInternetGateways" => (200, "<internetGatewayId>igw-9</internetGatewayId><attachmentSet><item><vpcId>vpc-9</vpcId></item></attachmentSet>".into()),
            "DescribeRouteTables" => (200, "<routeTableId>rtb-9</routeTableId>".into()),
            "DescribeAvailabilityZones" => (200, "<zoneName>us-east-1a</zoneName>".into()),
            "DescribeSubnets" => (200, "<subnetId>subnet-9</subnetId><cidrBlock>10.212.0.0/20</cidrBlock><availabilityZone>us-east-1a</availabilityZone>".into()),
            "DescribeVpcEndpoints" => (200, "<vpcEndpointId>vpce-9</vpcEndpointId>".into()),
            "DescribeSecurityGroups" => (200, "<groupId>sg-9</groupId>".into()),
            _ => ok(),
        }
    })
}

#[test]
fn a_network_made_holds_with_each_call_failing() {
    let n = swept(|| empty_region(2, None), |aws| futures::executor::block_on(make_network(aws, "us-east-1", &creds(), "p1", 0)));
    assert_eq!((n.vpc.as_str(), n.security_group.as_str(), n.subnets.len()), ("vpc-1", "sg-1", 2));
    // Another made at the same moment, with a lower id: that one is kept, this one removed.
    let n = swept(|| empty_region(1, Some("vpc-0")), |aws| futures::executor::block_on(make_network(aws, "us-east-1", &creds(), "p1", 0)));
    assert_eq!(n.vpc, "vpc-0");
    // More zones than the VPC has room for subnets: as many as there is room for.
    let n = once(empty_region(17, None), |aws| futures::executor::block_on(make_network(aws, "us-east-1", &creds(), "p1", 0)));
    assert_eq!(n.subnets.len(), 16);
    // One there already, whole (the control plane's own from before, and the shared one): nothing made.
    for own in [true, false] {
        let n = swept(|| whole_region(own), |aws| futures::executor::block_on(make_network(aws, "us-east-1", &creds(), "p1", 0)));
        assert_eq!((n.vpc.as_str(), n.security_group.as_str()), ("vpc-9", "sg-9"));
        let found = swept(|| whole_region(own), |aws| futures::executor::block_on(find_network(aws, "us-east-1", &creds(), "p1", 0)));
        assert_eq!(found, Some(n));
    }
    // None, and one without its group or its subnets: none found.
    assert_eq!(swept(|| empty_region(1, None), |aws| futures::executor::block_on(find_network(aws, "us-east-1", &creds(), "p1", 0))), None);
    let partial = |without: &'static str| -> Answer { let whole = whole_region(false); Box::new(move |w, r| if w == without { (200, String::new()) } else { whole(w, r) }) };
    assert_eq!(swept(|| partial("DescribeSecurityGroups"), |aws| futures::executor::block_on(find_network(aws, "us-east-1", &creds(), "p1", 0))), None);
    assert_eq!(swept(|| partial("DescribeSubnets"), |aws| futures::executor::block_on(find_network(aws, "us-east-1", &creds(), "p1", 0))), None);
}

/// EC2 holding a control plane's machines (until they are terminated, or for good: `stay`) and networks: its own
/// from before (`own`), the shared one, and `in_shared` a machine of another's in the shared one.
fn with_machines(own: bool, stay: bool, in_shared: bool) -> Answer {
    let ended = Cell::new(false);
    Box::new(move |w, r| {
        let asked = String::from_utf8_lossy(&r.body).to_string();
        match w {
            "DescribeInstances" if asked.contains("vpc-id") => (200, if in_shared { "<instanceId>i-other</instanceId>".into() } else { String::new() }),
            "DescribeInstances" if stay || !ended.get() => (200, "<instanceId>i-1</instanceId><instanceId>i-1</instanceId>".into()),
            "DescribeInstances" => (200, String::new()),
            "TerminateInstances" => { ended.set(true); ok() }
            "DescribeVpcs" if asked.contains("superci-network") => (200, "<vpcId>vpc-shared</vpcId>".into()),
            "DescribeVpcs" if own && asked.contains("p1") => (200, "<vpcId>vpc-own</vpcId>".into()),
            "DescribeVpcs" if asked.contains("other") => (200, "<vpcId>vpc-other</vpcId>".into()),
            "DescribeVpcs" => (200, String::new()),
            "DescribeSecurityGroups" => (200, "<groupId>sg-1</groupId>".into()),
            "DescribeSubnets" => (200, "<subnetId>subnet-1</subnetId>".into()),
            "DescribeVpcEndpoints" => (200, "<vpcEndpointId>vpce-1</vpcEndpointId>".into()),
            "DescribeRouteTables" => (200, "<routeTableId>rtb-1</routeTableId>".into()),
            "DescribeInternetGateways" => (200, "<internetGatewayId>igw-1</internetGatewayId>".into()),
            _ => ok(),
        }
    })
}

#[test]
fn a_network_deleted_holds_with_each_call_failing() {
    let deleted = |aws: &dyn Http, others: &[&str]| {
        let others: Vec<String> = others.iter().map(|o| o.to_string()).collect();
        let calls = std::cell::Cell::new(0u64);
        futures::executor::block_on(delete_network(aws, "us-east-1", &creds(), "p1", &others, &|| 0, &|_| calls.set(calls.get() + 1)))
    };
    // Its machines ended, its own network and the shared one (no other control plane uses it) removed.
    swept(|| with_machines(true, false, false), |aws| deleted(aws, &["other"]));
    // The shared one kept: another control plane with no network of its own uses it, or a machine is in it.
    swept(|| with_machines(false, false, false), |aws| deleted(aws, &["p2"]));
    swept(|| with_machines(false, false, true), |aws| deleted(aws, &[]));
    // Machines that do not end in the time given: the network is tried anyway.
    once(with_machines(true, true, false), |aws| deleted(aws, &[]));
    // Nothing there: nothing done.
    swept(|| Box::new(|_: &str, _: &Request| (200, String::new())), |aws| deleted(aws, &[]));
}

/// S3 holding a bucket (`there`, or not: its HEAD answered `head`), its objects listed `pages` times before it is
/// empty.
fn s3_answers(head: u16, pages: usize) -> Answer {
    let listed = Cell::new(0);
    Box::new(move |w, _| {
        let (method, url) = w.split_once(' ').unwrap_or_default();
        match method {
            "HEAD" => (head, String::new()),
            "GET" if listed.get() < pages => { listed.set(listed.get() + 1); (200, "<ListBucketResult><Contents><Key>a&amp;b</Key></Contents><Contents><Key>c</Key></Contents></ListBucketResult>".into()) }
            "GET" => (200, "<ListBucketResult></ListBucketResult>".into()),
            "DELETE" if url.ends_with("/c") => (404, String::new()),
            "DELETE" => (204, String::new()),
            _ => (200, String::new()),
        }
    })
}

#[test]
fn the_cache_bucket_made_filled_and_deleted_holds_with_each_call_failing() {
    for region in ["us-east-1", "eu-north-1"] {
        swept(|| s3_answers(404, 0), |aws| futures::executor::block_on(make_bucket(aws, region, &creds(), "p1", 0)));
        swept(|| s3_answers(200, 0), |aws| futures::executor::block_on(make_bucket(aws, region, &creds(), "p1", 0)));
    }
    // Not this sign-in's, or not answered: said so.
    for (head, said) in [(403, "AccessDenied"), (500, "HTTP 500")] {
        let e = futures::executor::block_on(make_bucket(&Aws { answer: s3_answers(head, 0), fail: None, n: Cell::new(0), calls: RefCell::default() }, "us-east-1", &creds(), "p1", 0)).unwrap_err();
        assert!(e.starts_with(said), "{e}");
    }
    swept(|| s3_answers(404, 0), |aws| futures::executor::block_on(put_once(aws, "us-east-1", &creds(), "p1", "k", b"body", 0)));
    // There as it is: not put again.
    let same: Answer = Box::new(|w, _| if w.starts_with("HEAD") { (200, String::new()) } else { (500, "<Error><Code>InternalError</Code></Error>".into()) });
    let aws = Aws { answer: same, fail: None, n: Cell::new(0), calls: RefCell::default() };
    assert!(futures::executor::block_on(put_once(&aws, "us-east-1", &creds(), "p1", "k", b"body", 0)).is_err(), "no sum kept: put again, and refused");
    let sum = sha256_hex(b"body");
    let kept: Answer = Box::new(move |w, _| if w.starts_with("HEAD") { (200, String::new()) } else { (500, String::new()) });
    struct Summed(Aws, String);
    #[async_trait::async_trait(?Send)]
    impl Http for Summed { async fn send(&self, r: Request) -> Result<crate::io::Response> { Ok(self.0.send(r).await?.with_header("x-amz-meta-sha256", &self.1)) } }
    let aws = Summed(Aws { answer: kept, fail: None, n: Cell::new(0), calls: RefCell::default() }, sum);
    futures::executor::block_on(put_once(&aws, "us-east-1", &creds(), "p1", "k", b"body", 0)).unwrap();
    assert_eq!(aws.0.calls.borrow().len(), 1);
    // Deleted with what is in it (two pages of it), and one that is gone already.
    swept(|| s3_answers(200, 2), |aws| futures::executor::block_on(delete_bucket(aws, "us-east-1", &creds(), "p1", &|| 0)));
    let gone: Answer = Box::new(|_, _| (404, String::new()));
    futures::executor::block_on(delete_bucket(&Aws { answer: gone, fail: None, n: Cell::new(0), calls: RefCell::default() }, "us-east-1", &creds(), "p1", &|| 0)).unwrap();
}

/// Prices as EC2 and the Price List answer them.
fn prices() -> Answer {
    Box::new(|w, r| {
        let asked = String::from_utf8_lossy(&r.body).to_string();
        match w {
            "DescribeSpotPriceHistory" if asked.contains("EndTime") => (200, "<item><timestamp>1970-01-01T00:00:00Z</timestamp><spotPrice>0.02</spotPrice></item><item><timestamp>1970-01-01T01:30:00Z</timestamp><spotPrice>0.04</spotPrice></item><item><timestamp>bad</timestamp></item>".into()),
            "DescribeSpotPriceHistory" => (200, "<item><instanceType>m7a.large</instanceType><availabilityZone>us-east-1a</availabilityZone><spotPrice>0.03</spotPrice></item><item><availabilityZone>us-east-1b</availabilityZone><spotPrice>0.05</spotPrice></item><item><spotPrice>x</spotPrice></item>".into()),
            "AWSPriceListService.GetProducts" => (200, serde_json::json!({ "PriceList": [serde_json::json!({ "terms": { "OnDemand": { "t": { "priceDimensions": { "d": { "pricePerUnit": { "USD": "0.0000000000" } }, "e": { "pricePerUnit": { "USD": "0.0864" } } } } } } }).to_string()] }).to_string()),
            "DescribeInstanceTypeOfferings" => (200, "<item><instanceType>m7a.large</instanceType></item>".into()),
            "DescribeImages" if asked.contains("tag") => (200, "<imageId>ami-0123456789abcdef0</imageId><name>n</name><imageState>available</imageState><snapshotId>snap-0123456789abcdef0</snapshotId><imageId>bad</imageId>".into()),
            "DescribeImages" => (200, "<imageId>ami-1</imageId><creationDate>2026-01-01</creationDate><volumeSize>30</volumeSize><imageId>ami-2</imageId><creationDate>2026-02-01</creationDate><imageId>ami-3</imageId>".into()),
            "CopyImage" => (200, "<imageId>ami-copy</imageId>".into()),
            "GetMetricStatistics" => (200, "<Sum>100</Sum><Sum>20.5</Sum><Sum>x</Sum>".into()),
            "ServiceQuotasV20190624.GetServiceQuota" => (400, serde_json::json!({ "__type": "NoSuchResourceException", "message": "none" }).to_string()),
            "ServiceQuotasV20190624.GetAWSDefaultServiceQuota" => (200, serde_json::json!({ "Quota": { "Value": 5.0 } }).to_string()),
            _ => ok(),
        }
    })
}

#[test]
fn prices_images_and_quotas_read_hold_with_each_call_failing() {
    let on = |f: &dyn Fn(&dyn Http) -> Result<f64>| swept(prices, f);
    assert_eq!(on(&|aws| futures::executor::block_on(spot_price(aws, "us-east-1", Some("us-east-1a"), &creds(), "m7a.large", "linux", 0))), 0.03);
    // Two hours and a half: the first price for an hour, then the next in effect at each hour's start.
    assert!((on(&|aws| futures::executor::block_on(spot_cost(aws, "us-east-1", "us-east-1a", &creds(), "m7a.large", "linux", 0, 9_000_000, 0))) - 0.02 * 2.0 - 0.04 * 0.5).abs() < 1e-9);
    assert_eq!(on(&|aws| futures::executor::block_on(on_demand_price(aws, &creds(), "us-east-1", "m7a.large", "windows", 0))), 0.0864);
    assert_eq!(on(&|aws| futures::executor::block_on(disk_price(aws, &creds(), "us-east-1", 0))), 0.0864);
    assert_eq!(on(&|aws| futures::executor::block_on(bytes_sent(aws, "us-east-1", &creds(), "i-1", 0, 1, 0))), 120.5);
    assert_eq!(swept(prices, |aws| futures::executor::block_on(cpu_quota(aws, "us-east-1", &creds(), SPOT_CPU_QUOTA, 0))), 5);
    let types = vec!["m7a.large".to_string(), "c7a.large".to_string()];
    assert_eq!(swept(prices, |aws| futures::executor::block_on(offered_types(aws, "us-east-1", &creds(), &types, 0))), ["m7a.large"]);
    assert_eq!(swept(prices, |aws| futures::executor::block_on(image_for(aws, "us-east-1", &creds(), "1", "n-*", None, 0))), ("ami-2".to_string(), 0, None));
    assert_eq!(swept(prices, |aws| futures::executor::block_on(copy_image(aws, "eu-north-1", &creds(), "us-east-1", "ami-1", "n", 0))), "ami-copy");
    let copies = swept(prices, |aws| futures::executor::block_on(copied_images(aws, "us-east-1", &creds(), 0)));
    assert_eq!(copies.len(), 1);
    swept(prices, |aws| futures::executor::block_on(delete_copied_image(aws, "us-east-1", &creds(), &copies[0], 0)));
    swept(prices, |aws| futures::executor::block_on(terminate_instance(aws, "us-east-1", &creds(), "i-1", 0)));
    swept(prices, |aws| futures::executor::block_on(delete_template(aws, "us-east-1", &creds(), "t", 0)));
    // Each pool's price, by type and zone; none to be had, AWS's failure.
    let priced = |answer: Answer| futures::executor::block_on(spot_prices(&Aws { answer, fail: None, n: Cell::new(0), calls: RefCell::default() }, "us-east-1", &creds(), &types, "linux", 0));
    assert!(priced(prices()).unwrap().iter().any(|((t, _), p)| t == "m7a.large" && (p - 0.03).abs() < 1e-9));
    assert!(priced(Box::new(|_, _| (500, String::new()))).is_err());
}

#[test]
fn answers_without_what_was_asked_for_are_said_as_that() {
    let said = |answer: Answer, f: &dyn Fn(&dyn Http) -> Result<String>| f(&Aws { answer, fail: None, n: Cell::new(0), calls: RefCell::default() }).unwrap_err();
    let empty = || -> Answer { Box::new(|_, _| (200, String::new())) };
    assert_eq!(said(empty(), &|aws| futures::executor::block_on(spot_price(aws, "us-east-1", None, &creds(), "m7a.large", "linux", 0)).map(|p| p.to_string())), "no spot price");
    assert_eq!(said(empty(), &|aws| futures::executor::block_on(spot_cost(aws, "us-east-1", "a", &creds(), "m7a.large", "linux", 0, 1, 0)).map(|p| p.to_string())), "no spot price for that time");
    assert!(said(empty(), &|aws| futures::executor::block_on(on_demand_price(aws, &creds(), "us-east-1", "m7a.large", "linux", 0)).map(|p| p.to_string())).starts_with("no on-demand price"));
    assert!(said(empty(), &|aws| futures::executor::block_on(disk_price(aws, &creds(), "us-east-1", 0)).map(|p| p.to_string())).starts_with("no gp3 price"));
    assert_eq!(said(empty(), &|aws| futures::executor::block_on(cpu_quota(aws, "us-east-1", &creds(), SPOT_CPU_QUOTA, 0)).map(|p| p.to_string())), "Service Quotas answered without a value");
    assert!(said(empty(), &|aws| futures::executor::block_on(image_for(aws, "us-east-1", &creds(), "1", "n", None, 0)).map(|p| p.0)).starts_with("no image 1/n"));
    assert_eq!(said(empty(), &|aws| futures::executor::block_on(copy_image(aws, "us-east-1", &creds(), "a", "b", "n", 0))), "CopyImage without an imageId");
    assert_eq!(said(empty(), &|aws| futures::executor::block_on(assume_role_with_web_identity(aws, "us-east-1", "arn", "t", "s", 0)).map(|c| c.access_key_id)), "STS response without AccessKeyId");
    let half: Answer = Box::new(|_, _| (200, "<AccessKeyId>A</AccessKeyId>".into()));
    assert_eq!(said(half, &|aws| futures::executor::block_on(assume_role_with_web_identity(aws, "us-east-1", "arn", "t", "s", 0)).map(|c| c.access_key_id)), "STS response without SecretAccessKey");
    let whole: Answer = Box::new(|_, _| (200, "<AccessKeyId>A</AccessKeyId><SecretAccessKey>S</SecretAccessKey>".into()));
    assert_eq!(futures::executor::block_on(assume_role_with_web_identity(&Aws { answer: whole, fail: None, n: Cell::new(0), calls: RefCell::default() }, "us-east-1", "arn", "t", "s", 0)).unwrap().session_token, None);
    // A dry run AWS would have let through, one it ran, and one it refused.
    let dry = |answer: Answer| futures::executor::block_on(could_run(&Aws { answer, fail: None, n: Cell::new(0), calls: RefCell::default() }, "us-east-1", &creds(), "ami-1", "m7a.large", true, 0));
    assert_eq!(dry(Box::new(|_, _| (412, "<Response><Errors><Error><Code>DryRunOperation</Code><Message>Request would have succeeded</Message></Error></Errors></Response>".into()))), Ok(()));
    assert_eq!(dry(empty()), Err("AWS started a machine although asked only whether it could".into()));
    assert!(dry(Box::new(|_, _| (403, "<Response><Errors><Error><Code>UnauthorizedOperation</Code><Message>no</Message></Error></Errors></Response>".into()))).unwrap_err().starts_with("UnauthorizedOperation"));
    // A function called without waiting: Lambda's refusal in its words (either spelling), or its status.
    let later = |status: u16, body: &'static str| futures::executor::block_on(invoke_later(&Aws { answer: Box::new(move |_, _| (status, body.into())), fail: None, n: Cell::new(0), calls: RefCell::default() }, "us-east-1", "f", &serde_json::json!({}), &creds(), 0));
    assert_eq!(later(202, ""), Ok(()));
    assert_eq!(later(403, r#"{"Message":"denied"}"#), Err("f: denied".into()));
    assert_eq!(later(404, r#"{"message":"no such function"}"#), Err("f: no such function".into()));
    assert_eq!(later(500, ""), Err("f: HTTP 500".into()));
    // An address that is none (a region with a space in it): said, nothing sent.
    let never = || Aws { answer: Box::new(|_, _| panic!("nothing is sent")), fail: None, n: Cell::new(0), calls: RefCell::default() };
    assert!(futures::executor::block_on(invoke_later(&never(), "a b", "f", &serde_json::json!({}), &creds(), 0)).is_err());
    assert!(futures::executor::block_on(ec2(&never(), "a b", &creds(), "DescribeVpcs", serde_json::json!({}), 0)).is_err());
    assert!(futures::executor::block_on(s3(&never(), "GET", "https://a b.s3.amazonaws.com/", "a b", &creds(), vec![], &[], 0)).is_err());
    assert!(futures::executor::block_on(json_call(&never(), "POST", "https://a b/", "x", "ssm", None, "application/json", None, &creds(), 0)).is_err());
    assert!(futures::executor::block_on(assume_role_with_web_identity(&never(), "a b/", "arn", "t", "s", 0)).is_err());
    assert!(sigv4("GET", "https://example.com:8443/x", &[], b"", &creds(), "us-east-1", "s3", "20260101T000000Z").unwrap().iter().any(|(k, v)| k == "host" && v == "example.com:8443"));
}

#[test]
fn a_network_of_the_accounts_own_is_looked_up_holding_with_each_call_failing() {
    let g = GivenNetwork { subnets: vec!["subnet-0123456789abcdef0".into(), "subnet-0123456789abcdef1".into()], security_groups: vec!["sg-0123456789abcdef0".into(), "sg-0123456789abcdef1".into()], private: true };
    let answer = |subnets: &'static str, groups: &'static str| -> Answer { Box::new(move |w, _| (200, if w == "DescribeSubnets" { subnets.into() } else { groups.into() })) };
    let both = "<subnetId>subnet-0123456789abcdef0</subnetId><availabilityZone>us-east-1a</availabilityZone><vpcId>vpc-1</vpcId><subnetId>subnet-0123456789abcdef1</subnetId><availabilityZone>us-east-1b</availabilityZone><vpcId>vpc-1</vpcId><subnetId>subnet-else</subnetId><availabilityZone>z</availabilityZone><vpcId>vpc-1</vpcId>";
    let groups = "<groupId>sg-0123456789abcdef0</groupId><groupId>sg-0123456789abcdef1</groupId>";
    let n = swept(|| answer(both, groups), |aws| futures::executor::block_on(given_network(aws, "us-east-1", &creds(), &g, 0)));
    assert_eq!((n.vpc.as_str(), n.subnets.len(), n.more_groups.len(), n.private), ("vpc-1", 2, 1, true));
    let refused = |subnets: &'static str, groups: &'static str| futures::executor::block_on(given_network(&Aws { answer: answer(subnets, groups), fail: None, n: Cell::new(0), calls: RefCell::default() }, "us-east-1", &creds(), &g, 0)).unwrap_err();
    assert!(refused("", groups).contains("no subnet subnet-0123456789abcdef0"));
    assert!(refused("<subnetId>subnet-0123456789abcdef0</subnetId><availabilityZone>a</availabilityZone><vpcId>vpc-1</vpcId><subnetId>subnet-0123456789abcdef1</subnetId><availabilityZone>b</availabilityZone><vpcId>vpc-2</vpcId>", groups).contains("more than one VPC"));
    assert!(refused(both, "").contains("no security group sg-0123456789abcdef0 in vpc-1"));
}

/// A launch of `types` (spot or not), in `network` if any.
fn launch<'a>(types: &'a [String], network: Option<&'a Network>, spot: bool, timer: Option<&'a dyn crate::io::Timer>, clock: Option<&'a dyn Clock>, skip: &'a [(String, String)]) -> Launch<'a> {
    Launch { region: "us-east-1", network, image: "ami-1", types, user_data: "#!/bin/bash\necho hi\n", disk_gb: 30, clock, skip, disk_mibs: 125, disk_iops: 3000, tags: &[], spot, os: "linux", avoid: &[], timer, jitter_ms: 0, fleet: true, store_disk: Some((20, 125)) }
}

struct NoWait;
#[async_trait::async_trait(?Send)]
impl crate::io::Timer for NoWait { async fn wake_in(&self, _ms: u64) -> Result<()> { Ok(()) } }
struct Still;
impl Clock for Still { fn now_ms(&self) -> u64 { 0 } }

/// EC2 with spot prices for `types` in two zones, refusing the first `refuse` machines asked for one by one (with
/// `refusal`), and answering a fleet as `fleet` says.
fn spot_region(refuse: usize, refusal: &'static str, fleet: &'static str) -> Answer {
    let asked = Cell::new(0);
    Box::new(move |w, _| match w {
        "DescribeSpotPriceHistory" => (200, ["m7a.large", "c7a.large", "m8a.large"].iter().flat_map(|t| ["us-east-1a", "us-east-1b"].map(|z| format!("<item><instanceType>{t}</instanceType><availabilityZone>{z}</availabilityZone><spotPrice>0.03</spotPrice></item>"))).collect()),
        "RunInstances" if asked.get() < refuse => { asked.set(asked.get() + 1); (400, format!("<Response><Errors><Error><Code>{refusal}</Code><Message>no</Message></Error></Errors></Response>")) }
        "RunInstances" => (200, "<instanceId>i-1</instanceId><availabilityZone>us-east-1b</availabilityZone>".into()),
        "CreateLaunchTemplate" => ok(),
        "CreateFleet" => (200, fleet.into()),
        _ => ok(),
    })
}

const FLEET_GOT: &str = "<fleetId>fleet-1</fleetId><fleetInstanceSet><item><instanceIds><item>i-2</item></instanceIds><instanceType>c7a.large</instanceType><subnetId>subnet-b</subnetId></item></fleetInstanceSet>";
const FLEET_NONE: &str = "<fleetId>fleet-1</fleetId><errorSet><item><errorCode>InsufficientInstanceCapacity</errorCode><errorMessage>none</errorMessage><launchTemplateAndOverrides><overrides><instanceType>c7a.large</instanceType><subnetId>subnet-b</subnetId></overrides></launchTemplateAndOverrides></item><item><errorCode>InvalidFleetConfiguration</errorCode><errorMessage>Your requested instance type is not supported in your requested Availability Zone</errorMessage><launchTemplateAndOverrides><overrides><instanceType>m8a.large</instanceType><availabilityZone>us-east-1a</availabilityZone></overrides></launchTemplateAndOverrides></item></errorSet>";

#[test]
fn a_machine_started_holds_with_each_call_failing() {
    let types: Vec<String> = ["m7a.large", "c7a.large", "m8a.large"].map(String::from).to_vec();
    let net = Network { vpc: "vpc-1".into(), security_group: "sg-1".into(), subnets: vec![("us-east-1a".into(), "subnet-a".into()), ("us-east-1b".into(), "subnet-b".into())], more_groups: vec!["sg-2".into()], private: true };
    let (timer, clock) = (NoWait, Still);
    let started = |aws: &dyn Http, l: &Launch| { let mut told = Told::default(); futures::executor::block_on(run_instance(aws, &creds(), l, 0, &mut told)).map(|m| (m.id, m.kind, m.zone, told.refused.len())) };
    // On-demand: the first type there is.
    let on_demand = launch(&types, None, false, Some(&timer), Some(&clock), &[]);
    assert_eq!(swept(|| spot_region(0, "", ""), |aws| started(aws, &on_demand)).0, "i-1");
    // Spot in its network: the first pool had none, the rest asked in one request, which has one.
    let spot = launch(&types, Some(&net), true, Some(&timer), Some(&clock), &[]);
    let got = swept(|| spot_region(1, "InsufficientInstanceCapacity", FLEET_GOT), |aws| started(aws, &spot));
    assert_eq!((got.0.as_str(), got.1.as_str(), got.2.as_str()), ("i-2", "c7a.large", "us-east-1b"));
    // None of them has one: said as no room, each pool's word kept.
    let none = swept(|| spot_region(1, "InsufficientInstanceCapacity", FLEET_NONE), |aws| started(aws, &spot).or_else(|e| if e.starts_with("InsufficientInstanceCapacity") { Ok(("none".into(), String::new(), String::new(), 0)) } else { Err(e) }));
    assert_eq!(none.0, "none");
    // Asked too fast at first (waited for), and in the default network by zone.
    let default_network = launch(&types, None, true, Some(&timer), Some(&clock), &[]);
    assert_eq!(swept(|| spot_region(2, "RequestLimitExceeded", FLEET_GOT), |aws| started(aws, &default_network)).0, "i-1");
    // A fleet refused outright (the role may not): one by one after.
    let refused = "<errorSet><item><errorCode>UnauthorizedOperation</errorCode><errorMessage>no</errorMessage></item></errorSet>";
    assert_eq!(swept(|| spot_region(1, "InsufficientInstanceCapacity", refused), |aws| started(aws, &spot)).0, "i-1");
    // Every pool skipped: no room, without asking.
    let skip: Vec<(String, String)> = types.iter().flat_map(|t| ["us-east-1a", "us-east-1b"].map(|z| (t.clone(), z.to_string()))).collect();
    let skipped = launch(&types, Some(&net), true, None, None, &skip);
    let e = started(&Aws { answer: spot_region(0, "", ""), fail: None, n: Cell::new(0), calls: RefCell::default() }, &skipped).unwrap_err();
    assert!(e.starts_with("InsufficientInstanceCapacity: AWS had no machine"), "{e}");
}

#[test]
fn constant_loops_and_helpers_give_what_they_did() {
    // (What became an iterator from a loop over a fixed list gives what the loop gave.)
    assert_eq!(hex(&signing_key("secret", "20260101", "us-east-1", "ec2")).len(), 64);
    let mut out = vec![];
    query_params(&serde_json::json!({ "A": [], "B": [1, { "C": null }] }), "", &mut out);
    assert_eq!(out, [("B.1".to_string(), "1".to_string())]);
}

/// AWS's answers with one header more (the error type a JSON API names in it).
struct Headed(Aws, &'static str, &'static str);
#[async_trait::async_trait(?Send)]
impl Http for Headed { async fn send(&self, r: Request) -> Result<crate::io::Response> { Ok(self.0.send(r).await?.with_header(self.1, self.2)) } }

fn answering(answer: Answer) -> Aws { Aws { answer, fail: None, n: Cell::new(0), calls: RefCell::default() } }

fn sts_answer() -> Answer { Box::new(|_, _| (200, "<AccessKeyId>A</AccessKeyId><SecretAccessKey>S</SecretAccessKey><SessionToken>T</SessionToken>".into())) }

#[test]
fn what_a_json_api_says_is_wrong_is_read_from_wherever_it_says_it() {
    let call = |aws: &dyn Http| futures::executor::block_on(json_call(aws, "POST", "https://lambda.us-east-1.amazonaws.com/x", "us-east-1", "lambda", None, "application/json", Some(&serde_json::json!({})), &creds(), 0)).unwrap_err();
    // Its header, the body's `Type` (only who is at fault), or nothing: its status.
    assert_eq!(call(&Headed(answering(Box::new(|_, _| (400, r#"{"message":"m"}"#.into()))), "x-amzn-errortype", "ResourceNotFoundException:http://internal")), "ResourceNotFoundException: m");
    assert_eq!(call(&answering(Box::new(|_, _| (400, r#"{"Type":"User","Message":"m"}"#.into())))), "User: m");
    assert_eq!(call(&answering(Box::new(|_, _| (502, "Bad gateway".into())))), "HTTP 502: Bad gateway");
    // STS refusing, and STS, Lambda and IAM each with every call failing in turn.
    let refused = answering(Box::new(|_, _| (403, "<ErrorResponse><Error><Code>AccessDenied</Code><Message>Not authorized</Message></Error></ErrorResponse>".into())));
    assert_eq!(futures::executor::block_on(assume_role_with_web_identity(&refused, "us-east-1", "arn", "t", "s", 0)).unwrap_err(), "AccessDenied: Not authorized");
    assert_eq!(swept(sts_answer, |aws| futures::executor::block_on(assume_role_with_web_identity(aws, "us-east-1", "arn", "t", "s", 0)).map(|c| c.session_token)), Some("T".to_string()));
    swept(|| Box::new(|_, _| (202, String::new())), |aws| futures::executor::block_on(invoke_later(aws, "us-east-1", "f", &serde_json::json!({}), &creds(), 0)));
    swept(|| Box::new(|_, _| ok()), |aws| futures::executor::block_on(iam(aws, &creds(), "GetRole", serde_json::json!({ "RoleName": "r" }), 0)));
}

#[test]
fn small_readings_of_aws_s_words_take_every_shape() {
    // Times cut short anywhere, or not numbers: none.
    for t in ["2026", "2026-01", "2026-01-01", "2026-01-01T00", "2026-01-01T00:00", "2026-01-01T00:00:0", "x026-01-01T00:00:00Z"] { assert_eq!(parse_iso_ms(t), None, "{t}") }
    assert_eq!(xml_tag("<a>open", "a"), None);
    assert!(presign("GET", "not an address", &creds(), "us-east-1", 60, 0).is_err());
    assert!(presign("GET", "https://b.s3.us-east-1.amazonaws.com/k?x=1", &creds(), "us-east-1", 60, 0).is_err());
    assert!(trust_policy("arn:aws:iam::1:oidc-provider/p", "p.example", "p1", "sts.amazonaws.com").to_string().contains("plane:p1"));
    assert!(runner_policy("111111111111", "p1").to_string().contains("p1"));
    assert_eq!((budget_name("p1"), budget_role_name("p1"), stop_policy_name("p1")), ("superci-p1".into(), "superci-budget-p1".into(), "superci-stop-p1".into()));
    assert!(spot_quota_link("eu-north-1").ends_with(SPOT_CPU_QUOTA));
    assert_eq!((cpus("t3.micro"), cpus("m7a.weird")), (1, 0));
    for (region, usd) in [("us-west-1", 0.096), ("eu-north-1", 0.0836), ("ap-south-1", 0.0912), ("ca-central-1", 0.088), ("eu-west-2", 0.0928), ("eu-central-1", 0.0952), ("eu-west-1", 0.088), ("us-east-1", 0.08), ("nowhere-1", 0.096)] { assert_eq!(disk_gb_month(region), usd) }
    assert_eq!(image_flavor("my-own-image-*"), "my-own-image");
    // A table's row with one cell, and a region that is none: left out.
    assert!(published_images("### `k`\n\n| Region | AMI ID |\n| only |\n| |\n| US-EAST-1 | `ami-0123456789abcdef0` |\n", "k").is_empty());
    // An organization's policy whose account number is none, and a policy not an organization's; a refusal said
    // without its code.
    let r = Refusal { action: "x".into(), by: By::Organization, policy: "arn:aws:organizations::12:policy/p-1".into() };
    assert_eq!((r.management_account(), Refusal { policy: "arn:aws:iam::aws:policy/x".into(), ..r.clone() }.management_account()), (None, None));
    assert!(refusal("Something: User x is not authorized to perform: ec2:RunInstances").is_some());
    // Prices: none for no type; the Price List's products as objects, and a price without its dollars.
    let none: Vec<String> = vec![];
    assert!(futures::executor::block_on(spot_prices(&answering(prices()), "us-east-1", &creds(), &none, "linux", 0)).unwrap().is_empty());
    let listed: Answer = Box::new(|_, _| (200, serde_json::json!({ "PriceList": [{ "terms": { "OnDemand": { "t": { "priceDimensions": { "d": { "pricePerUnit": {} }, "e": { "pricePerUnit": { "USD": "0.5" } } } } } } }] }).to_string()));
    assert_eq!(futures::executor::block_on(disk_price(&answering(listed), &creds(), "us-east-1", 0)).unwrap(), 0.5);
    let spotless: Answer = Box::new(|_, _| (200, "<item><spotPrice>0.1</spotPrice></item><item><timestamp>1970-01-01T00:00:00Z</timestamp></item><item><timestamp>1970-01-01T00:00:00Z</timestamp><spotPrice>x</spotPrice></item>".into()));
    assert!(futures::executor::block_on(spot_cost(&answering(spotless), "us-east-1", "a", &creds(), "m7a.large", "linux", 0, 1, 0)).is_err());
    swept(prices, |aws| futures::executor::block_on(spot_cpu_quota(aws, "us-east-1", &creds(), 0)));
    swept(|| Box::new(|_, _| (400, "<Response><Errors><Error><Code>DryRunOperation</Code><Message>ok</Message></Error></Errors></Response>".into())), |aws| futures::executor::block_on(could_run(aws, "us-east-1", &creds(), "ami-1", "m7a.large", false, 0)));
    // A copy without snapshots: only itself removed.
    swept(prices, |aws| futures::executor::block_on(delete_copied_image(aws, "us-east-1", &creds(), &CopiedImage { id: "ami-1".into(), name: String::new(), state: String::new(), snapshots: vec![] }, 0)));
    // A bucket whose last removal finds it gone.
    let gone_at_last: Answer = Box::new(|w, _| if w.starts_with("GET") { (200, "<ListBucketResult></ListBucketResult>".into()) } else { (404, String::new()) });
    futures::executor::block_on(delete_bucket(&answering(gone_at_last), "us-east-1", &creds(), "p1", &|| 0)).unwrap();
    // A given network: none of either, an id that is none, and a subnet AWS answers without its VPC.
    let given = |subnets: &[&str], groups: &[&str]| GivenNetwork { subnets: subnets.iter().map(|s| s.to_string()).collect(), security_groups: groups.iter().map(|s| s.to_string()).collect(), private: false };
    let never = || answering(Box::new(|_, _| panic!("nothing is asked")));
    assert!(futures::executor::block_on(given_network(&never(), "us-east-1", &creds(), &given(&["subnet-0123456789abcdef0"], &[]), 0)).unwrap_err().contains("needs a subnet and a security group"));
    assert!(futures::executor::block_on(given_network(&never(), "us-east-1", &creds(), &given(&["subnet-0123456789abcdef0"], &["sg-x"]), 0)).unwrap_err().contains("sg-x is not"));
    let no_vpc: Answer = Box::new(|_, _| (200, "<subnetId>subnet-0123456789abcdef0</subnetId><availabilityZone>a</availabilityZone><subnetId>subnet-0123456789abcdef0</subnetId><vpcId>vpc-1</vpcId>".into()));
    assert!(futures::executor::block_on(given_network(&answering(no_vpc), "us-east-1", &creds(), &given(&["subnet-0123456789abcdef0"], &["sg-0123456789abcdef0"]), 0)).unwrap_err().contains("no subnet"));
}

#[test]
fn a_machines_start_up_script_refuses_what_it_cannot_put_together() {
    for bad in ["http://a.example/x", "https://a.example/x y"] { assert!(runner_user_data_for(&crate::io::Work::GitHub { jit: "abc".into() }, 60, "linux", Some(bad)).is_err(), "{bad}") }
    assert!(runner_user_data_for(&crate::io::Work::GitLab { url: "https://gitlab.com".into(), token: "t".into() }, 60, "windows", None).unwrap_err().contains("GitHub jobs only"));
    assert!(runner_user_data_for(&crate::io::Work::GitHub { jit: "not base64!".into() }, 60, "windows", None).is_err());
    let gitlab = runner_user_data_for(&crate::io::Work::GitLab { url: "https://gitlab.com".into(), token: "t".into() }, 60, "linux", Some("https://a.example/n?x=1")).unwrap();
    assert!(gitlab.contains("pkill -TERM -f /tmp/gitlab-runner"));
    assert!(runner_user_data_for(&crate::io::Work::Fail { jit: "abc".into(), why: "eA==".into() }, 60, "linux", Some("https://a.example/n")).is_ok());
    // A script without its runner, or a link that is none: refused, as said.
    for e in [with_cache(String::new(), "https://a.example/c"), with_cache("x".into(), "https://a.example/c d"), with_cache_gitlab(String::new(), "https://a.example/c"), with_cache_gitlab("x".into(), "a b"),
        with_cache_windows(String::new(), "https://a.example/c"), with_cache_windows("x".into(), "a b"), with_hello(String::new(), "https://a.example/h"), with_hello("x".into(), "a b"),
        with_hello_windows(String::new(), "https://a.example/h"), with_hello_windows("x".into(), "a b"), with_keep(String::new()), with_keep("c='https://x'".into())] {
        assert!(e.is_err_and(|e| !e.is_empty()));
    }
    // A configuration's addresses: only those that are host names, each once, from any spelling of the key.
    use base64::Engine;
    let b64 = |v: serde_json::Value| base64::engine::general_purpose::STANDARD.encode(v.to_string());
    let jit = b64(serde_json::json!({ ".runner": b64(serde_json::json!({ "serverUrl": "https://a.example/x", "ServerUrl": "https://a.example/y", "serverUrlV2": "not an address" })) }));
    assert_eq!(runner_hosts(&jit), ["a.example"]);
    assert!(runner_hosts("not base64").is_empty());
    // The cache's link before its part when the script restarts no Docker.
    assert!(with_cache("cd /home/runner && run".into(), "https://a.example/c").unwrap().starts_with("c='https://a.example/c'\ncurl"));
}

#[test]
fn a_machine_not_to_be_had_is_said_as_that_in_every_way_aws_says_it() {
    let started = |answer: Answer, l: &Launch| { let mut told = Told::default(); futures::executor::block_on(run_instance(&answering(answer), &creds(), l, 0, &mut told)).map(|m| m.id) };
    // No type given; every pool priced over an on-demand machine; asked too fast with nothing to wait with.
    assert_eq!(started(spot_region(0, "", ""), &launch(&[], None, false, None, None, &[])).unwrap_err(), "no instance type given");
    let types: Vec<String> = ["m7a.large", "c7a.large", "m8a.large"].map(String::from).to_vec();
    let dear: Answer = Box::new(|w, _| if w == "DescribeSpotPriceHistory" { (200, "<item><instanceType>m7a.large</instanceType><availabilityZone>a</availabilityZone><spotPrice>9.5</spotPrice></item><item><instanceType>c7a.large</instanceType><availabilityZone>a</availabilityZone><spotPrice>9.5</spotPrice></item><item><instanceType>m8a.large</instanceType><availabilityZone>a</availabilityZone><spotPrice>9.5</spotPrice></item>".into()) } else { ok() });
    assert!(started(dear, &launch(&types, None, true, None, None, &[])).unwrap_err().contains("cost more than an on-demand machine"));
    assert!(started(spot_region(5, "RequestLimitExceeded", ""), &launch(&types, None, false, None, None, &[])).unwrap_err().starts_with("RequestLimitExceeded"));
    // Five types with no price known, in the default network: the first refused, the rest asked at once with no zone
    // named, without a clock; and a fleet that is refused for its rate every time.
    let five: Vec<String> = ["m7a.large", "c7a.large", "m8a.large", "c8a.large", "r7a.large"].map(String::from).to_vec();
    let unpriced = |fleet: &'static str| -> Answer { let a = spot_region(1, "InsufficientInstanceCapacity", fleet); Box::new(move |w, r| if w == "DescribeSpotPriceHistory" { (200, String::new()) } else { a(w, r) }) };
    assert_eq!(started(unpriced(FLEET_GOT), &launch(&five, None, true, None, None, &[])).unwrap(), "i-2");
    let timer = NoWait;
    let throttled: Answer = { let a = unpriced(""); Box::new(move |w, r| if w == "CreateFleet" { (400, "<Response><Errors><Error><Code>RequestLimitExceeded</Code><Message>slow down</Message></Error></Errors></Response>".into()) } else { a(w, r) }) };
    assert_eq!(started(throttled, &launch(&five, None, true, Some(&timer), None, &[])).unwrap(), "i-1", "one by one after the fleet could not be asked");
    // What a fleet says of pools it names without a type, or of a zone for another reason than the type: not kept,
    // or an error of its own.
    let mut told = Told::default();
    let pools = vec![("m7a.large".to_string(), Some("subnet-a".to_string()), "us-east-1a".to_string())];
    let nameless = "<fleetId>f</fleetId><errorSet><item><errorCode>UnfulfillableCapacity</errorCode><errorMessage>none</errorMessage><launchTemplateAndOverrides><overrides><subnetId>subnet-a</subnetId></overrides></launchTemplateAndOverrides></item></errorSet>";
    assert!(fleet_said(nameless, &pools, &mut told).unwrap().is_err() && told.refused.is_empty());
    let other = "<fleetId>f</fleetId><errorSet><item><errorCode>InvalidFleetConfiguration</errorCode><errorMessage>Something else</errorMessage></item></errorSet>";
    assert_eq!(fleet_said(other, &pools, &mut told).err().as_deref(), Some("InvalidFleetConfiguration: Something else"));
    assert_eq!(fleet_said("<nothing/>", &pools, &mut told).err().as_deref(), Some("CreateFleet without a fleet"));
}
