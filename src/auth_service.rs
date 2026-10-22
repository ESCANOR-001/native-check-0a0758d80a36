use crate::auth::Auth;
use tokio::sync::{mpsc, oneshot};

type Operation = Box<dyn FnOnce(&mut Auth) + Send>;

/// One owner serializes database mutations off the network runtime. A bounded
/// mailbox prevents slow storage from creating an unbounded blocking pool.
#[derive(Clone)]
pub struct AuthService(mpsc::Sender<Operation>);
impl AuthService {
    pub fn new(mut auth: Auth) -> std::io::Result<Self> {
        let (send, mut receive) = mpsc::channel::<Operation>(16);
        std::thread::Builder::new()
            .name("remodex-auth".to_owned())
            .spawn(move || {
                while let Some(operation) = receive.blocking_recv() {
                    operation(&mut auth);
                }
            })?;
        Ok(Self(send))
    }
    pub async fn call<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Auth) -> T + Send + 'static,
    ) -> Result<T, &'static str> {
        let (send, receive) = oneshot::channel();
        self.0
            .try_send(Box::new(move |auth| {
                let _ = send.send(operation(auth));
            }))
            .map_err(|_| "Authorization service busy or unavailable")?;
        receive.await.map_err(|_| "Authorization service stopped")
    }
}
