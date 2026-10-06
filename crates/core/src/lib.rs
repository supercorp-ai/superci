//! superci-core: the runtime-neutral core of a SuperCI plane. A control plane lives in the user's own cloud account
//! (a Cloudflare Worker and Durable Object, or AWS Lambda), creates its own GitHub App, and runs GitHub Actions jobs
//! on machines in connected clouds. Runtimes provide HTTP, storage, time and a timer (`io`); everything else is here.
pub mod aws;
pub mod crypto;
pub mod docker;
pub mod github;
pub mod gitlab;
pub mod plane;
pub mod io;
pub mod page;
pub mod permissions;
pub mod spec;
