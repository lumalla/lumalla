use calloop::{LoopHandle, LoopSignal};
use lumalla_shared::{MessageRunner, RendererMessage};

pub struct RendererState {}

impl MessageRunner for RendererState {
    type Message = RendererMessage;

    fn new(
        _comms: lumalla_shared::Comms,
        _loop_handle: LoopHandle<'static, Self>,
        _args: std::sync::Arc<lumalla_shared::GlobalArgs>,
    ) -> anyhow::Result<Self>
    where
        Self: Sized,
    {
        Ok(Self {})
    }

    fn handle_message(&mut self, _message: Self::Message) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_dispatch_wait(&mut self, _signal: &LoopSignal) {}
}
