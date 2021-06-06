use crate::{
    body::{Body, BoxBody},
    extract::FromRequest,
    response::IntoResponse,
    routing::{BoxResponseBody, EmptyRouter, MethodFilter},
    service::HandleError,
};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::future;
use http::{Request, Response};
use std::{
    convert::Infallible,
    future::Future,
    marker::PhantomData,
    task::{Context, Poll},
};
use tower::{util::Oneshot, BoxError, Layer, Service, ServiceExt};

pub fn get<H, B, T>(handler: H) -> OnMethod<IntoService<H, B, T>, EmptyRouter>
where
    H: Handler<B, T>,
{
    on(MethodFilter::Get, handler)
}

pub fn post<H, B, T>(handler: H) -> OnMethod<IntoService<H, B, T>, EmptyRouter>
where
    H: Handler<B, T>,
{
    on(MethodFilter::Post, handler)
}

pub fn on<H, B, T>(method: MethodFilter, handler: H) -> OnMethod<IntoService<H, B, T>, EmptyRouter>
where
    H: Handler<B, T>,
{
    OnMethod {
        method,
        svc: handler.into_service(),
        fallback: EmptyRouter,
    }
}

mod sealed {
    #![allow(unreachable_pub)]

    pub trait HiddentTrait {}
    pub struct Hidden;
    impl HiddentTrait for Hidden {}
}

#[async_trait]
pub trait Handler<B, In>: Sized {
    type Response: IntoResponse<B>;

    // This seals the trait. We cannot use the regular "sealed super trait" approach
    // due to coherence.
    #[doc(hidden)]
    type Sealed: sealed::HiddentTrait;

    async fn call(self, req: Request<Body>) -> Response<B>;

    fn layer<L>(self, layer: L) -> Layered<L::Service, In>
    where
        L: Layer<IntoService<Self, B, In>>,
    {
        Layered::new(layer.layer(IntoService::new(self)))
    }

    fn into_service(self) -> IntoService<Self, B, In> {
        IntoService::new(self)
    }
}

#[async_trait]
impl<F, Fut, B, Res> Handler<B, ()> for F
where
    F: FnOnce(Request<Body>) -> Fut + Send + Sync,
    Fut: Future<Output = Res> + Send,
    Res: IntoResponse<B>,
{
    type Response = Res;

    type Sealed = sealed::Hidden;

    async fn call(self, req: Request<Body>) -> Response<B> {
        self(req).await.into_response()
    }
}

macro_rules! impl_handler {
    () => {};

    ( $head:ident, $($tail:ident),* $(,)? ) => {
        #[async_trait]
        #[allow(non_snake_case)]
        impl<F, Fut, B, Res, $head, $($tail,)*> Handler<B, ($head, $($tail,)*)> for F
        where
            F: FnOnce(Request<Body>, $head, $($tail,)*) -> Fut + Send + Sync,
            Fut: Future<Output = Res> + Send,
            Res: IntoResponse<B>,
            $head: FromRequest<B> + Send,
            $( $tail: FromRequest<B> + Send, )*
        {
            type Response = Res;

            type Sealed = sealed::Hidden;

            async fn call(self, mut req: Request<Body>) -> Response<B> {
                let $head = match $head::from_request(&mut req).await {
                    Ok(value) => value,
                    Err(rejection) => return rejection.into_response(),
                };

                $(
                    let $tail = match $tail::from_request(&mut req).await {
                        Ok(value) => value,
                        Err(rejection) => return rejection.into_response(),
                    };
                )*

                let res = self(req, $head, $($tail,)*).await;

                res.into_response()
            }
        }

        impl_handler!($($tail,)*);
    };
}

impl_handler!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16);

pub struct Layered<S, T> {
    svc: S,
    _input: PhantomData<fn() -> T>,
}

impl<S, T> Clone for Layered<S, T>
where
    S: Clone,
{
    fn clone(&self) -> Self {
        Self::new(self.svc.clone())
    }
}

#[async_trait]
impl<S, B, T> Handler<B, T> for Layered<S, T>
where
    S: Service<Request<Body>, Response = Response<B>> + Send,
    S::Error: IntoResponse<B>,
    S::Response: IntoResponse<B>,
    S::Future: Send,
{
    type Response = S::Response;

    type Sealed = sealed::Hidden;

    async fn call(self, req: Request<Body>) -> Self::Response {
        // TODO(david): add tests for nesting services
        match self
            .svc
            .oneshot(req)
            .await
            .map_err(IntoResponse::into_response)
        {
            Ok(res) => res,
            Err(res) => res,
        }
    }
}

impl<S, T> Layered<S, T> {
    pub(crate) fn new(svc: S) -> Self {
        Self {
            svc,
            _input: PhantomData,
        }
    }

    pub fn handle_error<F, B, Res>(self, f: F) -> Layered<HandleError<S, F>, T>
    where
        S: Service<Request<Body>, Response = Response<B>>,
        F: FnOnce(S::Error) -> Res,
        Res: IntoResponse<B>,
        B: http_body::Body<Data = Bytes> + Send + Sync + 'static,
        B::Error: Into<BoxError> + Send + Sync + 'static,
    {
        let svc = HandleError::new(self.svc, f);
        Layered::new(svc)
    }
}

pub struct IntoService<H, B, T> {
    handler: H,
    _marker: PhantomData<fn() -> (B, T)>,
}

impl<H, B, T> IntoService<H, B, T> {
    fn new(handler: H) -> Self {
        Self {
            handler,
            _marker: PhantomData,
        }
    }
}

impl<H, B, T> Clone for IntoService<H, B, T>
where
    H: Clone,
{
    fn clone(&self) -> Self {
        Self {
            handler: self.handler.clone(),
            _marker: PhantomData,
        }
    }
}

impl<H, B, T> Service<Request<Body>> for IntoService<H, B, T>
where
    H: Handler<B, T> + Clone + Send + 'static,
    H::Response: 'static,
{
    type Response = Response<B>;
    type Error = Infallible;
    type Future = future::BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // `IntoService` can only be constructed from async functions which are always ready, or from
        // `Layered` which bufferes in `<Layered as Handler>::call` and is therefore also always
        // ready.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let handler = self.handler.clone();
        Box::pin(async move { Ok(Handler::call(handler, req).await) })
    }
}

#[derive(Clone)]
pub struct OnMethod<S, F> {
    pub(crate) method: MethodFilter,
    pub(crate) svc: S,
    pub(crate) fallback: F,
}

impl<S, F> OnMethod<S, F> {
    pub fn get<H, B, T>(self, handler: H) -> OnMethod<IntoService<H, B, T>, Self>
    where
        H: Handler<B, T>,
    {
        self.on(MethodFilter::Get, handler)
    }

    pub fn post<H, B, T>(self, handler: H) -> OnMethod<IntoService<H, B, T>, Self>
    where
        H: Handler<B, T>,
    {
        self.on(MethodFilter::Post, handler)
    }

    pub fn on<H, B, T>(
        self,
        method: MethodFilter,
        handler: H,
    ) -> OnMethod<IntoService<H, B, T>, Self>
    where
        H: Handler<B, T>,
    {
        OnMethod {
            method,
            svc: handler.into_service(),
            fallback: self,
        }
    }
}

// this is identical to `routing::OnMethod`'s implementation. Would be nice to find a way to clean
// that up, but not sure its possible.
impl<S, F, SB, FB> Service<Request<Body>> for OnMethod<S, F>
where
    S: Service<Request<Body>, Response = Response<SB>, Error = Infallible> + Clone,
    SB: http_body::Body<Data = Bytes> + Send + Sync + 'static,
    SB::Error: Into<BoxError>,

    F: Service<Request<Body>, Response = Response<FB>, Error = Infallible> + Clone,
    FB: http_body::Body<Data = Bytes> + Send + Sync + 'static,
    FB::Error: Into<BoxError>,
{
    type Response = Response<BoxBody>;
    type Error = Infallible;

    #[allow(clippy::type_complexity)]
    type Future = future::Either<
        BoxResponseBody<Oneshot<S, Request<Body>>>,
        BoxResponseBody<Oneshot<F, Request<Body>>>,
    >;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        if self.method.matches(req.method()) {
            let response_future = self.svc.clone().oneshot(req);
            future::Either::Left(BoxResponseBody(response_future))
        } else {
            let response_future = self.fallback.clone().oneshot(req);
            future::Either::Right(BoxResponseBody(response_future))
        }
    }
}