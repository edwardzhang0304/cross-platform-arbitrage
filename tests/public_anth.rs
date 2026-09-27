#![cfg(feature="paper-runtime")]
use openai_paired_trader::openai_inventory::{paper_funding,MarketPair,Venue};
#[tokio::test]
#[ignore="requires public market endpoints, no accounts or credentials"]
async fn public_anth_funding_history_decodes_both_venues() {
    let hour=paper_funding::HOUR;
    let at=(openai_paired_trader::domain::now_ms()/hour-1)*hour;
    for venue in [Venue::Lighter,Venue::Entropy] {
        let rows=paper_funding::per_base(venue,MarketPair::Anth,at,at).await.unwrap();
        assert_eq!(rows.len(),1);assert_eq!(rows[0].0,at);
        println!("{venue:?}: one public hourly settlement decoded; no account endpoint used");
    }
}
