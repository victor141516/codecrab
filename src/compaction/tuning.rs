/// Centralized production tuning for automatic context compaction.
///
/// These values are deliberately not user configuration yet. Keeping them in
/// one typed structure makes policy experiments and small deterministic tests
/// possible without scattering unexplained literals through the agent loop.
#[derive(Clone, Debug)]
pub(crate) struct CompactionTuning {
    /// Percentage of an advertised context window used when the provider does
    /// not publish an auto-compaction limit. Eighty percent leaves more margin
    /// than mature Codex deployments; raising it uses context more efficiently
    /// but increases overflow risk, while lowering it compacts earlier and more
    /// often. Provider metadata overrides this fallback.
    pub fallback_context_percent: u64,
    /// Context window assumed when neither catalog nor manual configuration
    /// publishes one. 128k is conservative for current coding models; raising
    /// it risks overflowing smaller unknown models, while lowering it causes
    /// premature lossy summaries. This is only a metadata fallback.
    pub fallback_context_window_tokens: u64,
    /// Absolute fallback space kept free for request growth and summary
    /// recovery. 16k accommodates a useful answer plus protocol growth;
    /// increasing it improves reliability at the cost of usable history, while
    /// decreasing it delays compaction but raises overflow risk.
    pub safety_reserve_tokens: u64,
    /// Smallest local output allowance subtracted from the context window even
    /// when the provider advertises a lower maximum output. 8k supports normal
    /// coding answers; larger values reduce overflow risk but compact sooner,
    /// while smaller values expose more history but can crowd out the answer.
    pub minimum_output_reserve_tokens: u64,
    /// Preferred local token budget for the recent raw tail. 24k preserves
    /// several typical coding turns; increasing it improves exact-state
    /// fidelity but leaves less room for new work, while decreasing it makes
    /// summaries cheaper but more lossy.
    pub recent_tail_tokens: u64,
    /// Most aggressive local recent-tail budget used during overflow recovery.
    /// 8k usually retains the current turn; lowering it recovers more often but
    /// loses exact context, while raising it preserves fidelity but may fail to
    /// escape an overflow.
    pub minimum_recent_tail_tokens: u64,
    /// Local upper bound for adaptive tail policies. 32k prevents large-window
    /// models from retaining an unnecessarily expensive tail; raising it can
    /// improve fidelity, while lowering it reduces recurring request cost.
    pub maximum_recent_tail_tokens: u64,
    /// Local minimum number of complete user turns retained raw. One guarantees
    /// the in-progress protocol group remains intact; increasing it preserves
    /// more dialogue but can block recovery, while decreasing it to zero could
    /// orphan state required by the next request.
    pub minimum_recent_turns: usize,
    /// Local maximum summary output requested from the active model. 8k allows
    /// a detailed handoff; larger summaries retain more facts but make later
    /// requests expensive, while smaller summaries are cheaper but more lossy.
    pub maximum_summary_output_tokens: u64,
    /// Smallest local input budget allowed for one summary chunk after output
    /// reserves are removed. 16k avoids producing uselessly tiny chunks;
    /// increasing it reduces the number of summaries but can overflow small
    /// models, while decreasing it costs more rolling-summary calls.
    pub minimum_summarizer_input_tokens: u64,
    /// Local character limit for one historical tool result in the summarizer
    /// input. 12k keeps useful command/file edges and never changes canonical
    /// data; raising it improves fidelity but can overflow the summarizer,
    /// while lowering it makes durable facts easier to miss.
    pub summarizer_tool_output_characters: usize,
    /// Size above which a `read_file` result is omitted entirely from the
    /// summarizer input. 4k keeps small configuration files inline but makes
    /// large source files cheap to re-read; raising it improves immediate
    /// fidelity, while lowering it causes more follow-up reads.
    pub summarizer_file_content_characters: usize,
    /// Size above which large string fields in file-writing tool arguments are
    /// replaced with length markers. 4k preserves small edits; raising it can
    /// bloat compaction requests, while lowering it hides more exact patches.
    pub summarizer_tool_argument_characters: usize,
    /// Local token reduction requested from the retained raw tail after a
    /// provider overflow. 8k makes a meaningful second attempt; larger
    /// reductions recover faster but lose exact context, while smaller ones
    /// preserve quality but may repeat the overflow.
    pub overflow_recovery_reduction_tokens: u64,
    /// Local maximum emergency compactions for one provider request. Two covers
    /// a normal and a more aggressive recovery; more attempts can rescue
    /// unusual histories but risk costly loops, while fewer fail faster.
    pub maximum_overflow_recoveries: usize,
    /// Local retry count for a failed summary request. Two tolerates transient
    /// failures without hiding persistent errors; increasing it costs time and
    /// tokens, while decreasing it makes compaction less resilient.
    pub maximum_summary_retries: usize,
    /// Maximum number of successful rolling-summary chunks produced during one
    /// preflight. Eight handles very long histories without an unbounded loop;
    /// increasing it recovers more in one turn at higher latency, while
    /// decreasing it may leave the next request oversized.
    pub maximum_compaction_chunks_per_preflight: usize,
    /// Local growth required before retrying compaction after a success or
    /// failure. 4k absorbs a modest tool result; increasing it reduces repeated
    /// lossy calls but may approach overflow, while decreasing it reacts sooner
    /// at higher cost.
    pub hysteresis_tokens: u64,
    /// Conservative local character-to-token divisor used only when provider
    /// usage is unavailable. Four approximates English/code; lowering it
    /// estimates more tokens and compacts earlier, while raising it uses more
    /// context but underestimates dense tokenization.
    pub estimated_characters_per_token: u64,
    /// Local per-message estimate for role/protocol framing when usage is
    /// unavailable. Eight is a conservative small envelope; increasing it is
    /// safer for many short messages, while decreasing it delays compaction.
    pub estimated_tokens_per_message: u64,
    /// Local per-tool-call estimate beyond serialized names and arguments when
    /// usage is unavailable. Sixteen covers typical wrappers; increasing it is
    /// safer for verbose providers, while decreasing it exposes more context at
    /// greater undercount risk.
    pub estimated_tokens_per_tool_call: u64,
}

impl Default for CompactionTuning {
    fn default() -> Self {
        Self {
            fallback_context_percent: 80,
            fallback_context_window_tokens: 128_000,
            safety_reserve_tokens: 16_000,
            minimum_output_reserve_tokens: 8_000,
            recent_tail_tokens: 24_000,
            minimum_recent_tail_tokens: 8_000,
            maximum_recent_tail_tokens: 32_000,
            minimum_recent_turns: 1,
            maximum_summary_output_tokens: 8_000,
            minimum_summarizer_input_tokens: 16_000,
            summarizer_tool_output_characters: 12_000,
            summarizer_file_content_characters: 4_000,
            summarizer_tool_argument_characters: 4_000,
            overflow_recovery_reduction_tokens: 8_000,
            maximum_overflow_recoveries: 2,
            maximum_summary_retries: 2,
            maximum_compaction_chunks_per_preflight: 8,
            hysteresis_tokens: 4_000,
            estimated_characters_per_token: 4,
            estimated_tokens_per_message: 8,
            estimated_tokens_per_tool_call: 16,
        }
    }
}
