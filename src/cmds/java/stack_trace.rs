//! Port of maven-mcp's StackTraceProcessor.
//!
//! Parses Java stack traces into segments (top-level exception + Caused by
//! chains), classifies frames as application or framework by package prefix,
//! collapses framework noise, and preserves root-cause frames.
