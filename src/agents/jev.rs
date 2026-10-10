//! Shadow scoring with TypeSafe's Jev. Jev scores catalyst and news sentiment
//! from the raw headlines on the same 1–5 scale the research agent uses.
//! Shadow scores are recorded next to the agent's scores and shown to the
//! executor, so the two can be compared against outcomes before either is
//! trusted over the other.

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use typesafe::{Client, QuestionSet, ScoreAnswer, ScoreKey};

use crate::market::alpaca::NewsArticle;

/// A Jev score on the factor scale.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShadowScore {
    /// Probability-weighted, 1.0 (very bearish) to 5.0 (very bullish).
    pub score: f64,
    /// Jev's certainty in the score, 0–1.
    pub confidence: f64,
}

impl From<&ScoreAnswer> for ShadowScore {
    /// Jev scores rubric levels from 0; ours start at 1.
    fn from(answer: &ScoreAnswer) -> Self {
        Self {
            score: (answer.score + 1.0).clamp(1.0, 5.0),
            confidence: answer.confidence,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShadowScores {
    pub model: String,
    pub catalyst: ShadowScore,
    pub news_sentiment: ShadowScore,
    /// Headlines Jev saw.
    pub articles: usize,
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

/// The rubric, mirroring the research agent's 1–5 bullishness scale.
const LEVELS: [&str; 5] = [
    "Clearly bearish",
    "Leaning bearish",
    "Neutral, mixed, or no signal",
    "Leaning bullish",
    "Clearly bullish",
];

pub struct JevScorer {
    client: Client,
    questions: QuestionSet,
    catalyst: ScoreKey,
    news_sentiment: ScoreKey,
}

impl JevScorer {
    /// Reads the API key from `JEV_API_KEY`, falling back to the client's
    /// own `TYPESAFE_API_KEY`.
    pub fn from_env(model: &str) -> Result<Self> {
        let mut builder = Client::builder().default_model(model.to_string());
        if let Ok(key) = std::env::var("JEV_API_KEY") {
            builder = builder.api_key(key);
        }
        let client = builder.build().context("configuring the Jev client")?;

        let mut questions = QuestionSet::new();
        let catalyst = questions.score(
            "catalyst",
            "For a swing trade over the next two weeks: how strong, specific, and \
             timely is a catalyst for this stock (earnings, guidance, product or \
             regulatory events, deals, analyst actions, macro events)? Judge the \
             direction it pushes the stock. No specific catalyst is neutral.",
            LEVELS,
        );
        let news_sentiment = questions.score(
            "news_sentiment",
            "Overall, how bullish or bearish is the tone and substance of this \
             stock's recent news for its price over the next two weeks?",
            LEVELS,
        );
        Ok(Self {
            client,
            questions,
            catalyst,
            news_sentiment,
        })
    }

    /// Score `symbol` from its recent news as of `as_of`.
    pub async fn score(
        &self,
        symbol: &str,
        articles: &[NewsArticle],
        as_of: DateTime<Utc>,
    ) -> Result<ShadowScores> {
        let state = json!({
            "symbol": symbol,
            "as_of": as_of.format("%Y-%m-%d").to_string(),
            "recent_news": articles
                .iter()
                .map(|a| json!({
                    "date": a.created_at.format("%Y-%m-%d").to_string(),
                    "headline": a.headline,
                    "summary": a.summary,
                    "symbols": a.symbols,
                }))
                .collect::<Vec<_>>(),
        });
        let answers = self
            .client
            .system_one(&self.questions)
            .state(&state)
            .send()
            .await
            .with_context(|| format!("Jev scoring {symbol}"))?;
        Ok(ShadowScores {
            model: answers.model().to_string(),
            catalyst: ShadowScore::from(&answers[self.catalyst]),
            news_sentiment: ShadowScore::from(&answers[self.news_sentiment]),
            articles: articles.len(),
            input_tokens: answers.usage().input_tokens,
            output_tokens: answers.usage().output_tokens,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scores the latest AAPL news with Jev (fractions of a cent); run with
    /// `cargo test live_jev -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_jev() {
        dotenvy::dotenv().ok();
        let alpaca = crate::market::alpaca::AlpacaClient::from_env().unwrap();
        let jev = JevScorer::from_env("jev-latest").unwrap();
        let scores = crate::runtime::RUNTIME
            .block_on(async {
                let news = alpaca
                    .news(
                        &["AAPL".into()],
                        Utc::now() - chrono::Duration::days(14),
                        None,
                        30,
                    )
                    .await?;
                jev.score("AAPL", &news, Utc::now()).await
            })
            .unwrap();
        println!("{scores:#?}");
        assert!((1.0..=5.0).contains(&scores.catalyst.score));
    }
}
