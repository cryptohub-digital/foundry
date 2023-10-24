use crate::{config::*, test_helpers::filter::Filter};
use forge::revm::primitives::SpecId;

#[tokio::test(flavor = "multi_thread")]
async fn test_shanghai_compat() {
    let filter = Filter::new("", "ShanghaiCompat", ".*spec");
    TestConfig::filter(filter).await.evm_spec(SpecId::LATEST).run().await;
}
