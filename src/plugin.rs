use handlebars::{Handlebars, Helper, RenderContext, Output, RenderError, Context as HbContext};

pub trait HelperPlugin: Send + Sync {
    fn register(&self, hb: &mut Handlebars<'_>);
    fn name(&self) -> &str { "unnamed_plugin" }
}

pub type PluginFactory = fn() -> Box<dyn HelperPlugin>;

pub fn make_helper<F>(func: F) -> Box<dyn for<'a> Fn(
    &Helper<'a>,
    &Handlebars<'a>,
    &HbContext,
    &mut RenderContext<'a, '_>,
    &mut dyn Output
) -> Result<(), RenderError> + Send + Sync>
where
    F: Fn(&Helper<'_>, &Handlebars<'_>, &HbContext, &mut RenderContext<'_, '_>, &mut dyn Output) -> Result<(), RenderError> + Send + Sync + 'static,
{
    Box::new(func)
}