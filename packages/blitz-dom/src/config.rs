use crate::HtmlParserProvider;
use blitz_traits::{
    navigation::NavigationProvider,
    net::{AbortSignal, NetProvider},
    shell::{ShellProvider, Viewport},
};
use parley::FontContext;
use std::sync::Arc;
use style::media_queries::MediaType;

/// Strategy for Stylo's style traversal during `resolve`.
///
/// Two `Document`s resolving on [`StyleThreading::Parallel`] concurrently
/// share Stylo's global thread pool and used to panic with
/// `already mutably borrowed` — see
/// <https://github.com/DioxusLabs/blitz/issues/430>. That is why the
/// default was quietly [`Sequential`](Self::Sequential) while the
/// documentation here said otherwise, and it meant every document
/// resolved its style on one thread: on a tree of a few thousand nodes
/// that is most of a frame.
///
/// `Parallel` no longer has to be traded against that. A document takes
/// the global pool only if no other document is using it, and otherwise
/// traverses sequentially for that frame rather than panicking — so the
/// fast path is the default and the slow path is a fallback instead of a
/// decision the caller has to make correctly.
///
/// [`Sequential`](Self::Sequential) remains for a caller that wants the
/// pool left alone entirely.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum StyleThreading {
    /// Use Stylo's parallel traversal via its global rayon thread pool,
    /// falling back to a sequential traversal for any frame where
    /// another document already holds it.
    #[default]
    Parallel,
    /// Always traverse sequentially on the calling thread, bypassing the
    /// global pool.
    Sequential,
}

/// Options used when constructing a [`BaseDocument`](crate::BaseDocument)
#[derive(Default)]
pub struct DocumentConfig {
    /// The initial `Viewport`
    pub viewport: Option<Viewport>,
    /// The base url which relative URLs are resolved against
    pub base_url: Option<String>,
    /// User Agent stylesheets
    pub ua_stylesheets: Option<Vec<String>>,
    /// Net provider to handle network requests for resources
    pub net_provider: Option<Arc<dyn NetProvider>>,
    /// Navigation provider to handle link clicks and form submissions
    pub navigation_provider: Option<Arc<dyn NavigationProvider>>,
    /// Shell provider to redraw requests, clipboard, etc
    pub shell_provider: Option<Arc<dyn ShellProvider>>,
    /// HTML parser provider. Used to parse HTML for setInnerHTML
    pub html_parser_provider: Option<Arc<dyn HtmlParserProvider>>,
    /// Parley `FontContext`
    pub font_ctx: Option<FontContext>,
    /// The CSS media type used to evaluate `@media` rules.
    /// Defaults to [`MediaType::screen`].
    pub media_type: Option<MediaType>,
    /// Strategy for Stylo's style traversal.
    /// Defaults to [`StyleThreading::Parallel`].
    pub style_threading: StyleThreading,
    /// If set, every sub-resource `Request` blitz-dom creates for this
    /// document will carry this signal. Aborting it cancels every in-flight
    /// fetch tied to this document.
    pub abort_signal: Option<AbortSignal>,
}
