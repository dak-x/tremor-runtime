// Copyright 2020-2021, The Tremor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::{
    connectors::{self, ConnectorResult, KnownConnectors},
    errors::{Error, Result},
    instance::InstanceState,
    permge::PriorityMerge,
    pipeline,
};
use async_std::{channel::Receiver, prelude::*};
use async_std::{
    channel::{bounded, unbounded},
    task,
};
use hashbrown::HashMap;
use std::{borrow::Borrow, collections::HashSet};
use std::{sync::atomic::Ordering, time::Duration};
use tremor_common::{
    ids::{ConnectorIdGen, OperatorIdGen},
    url::TremorUrl,
};
use tremor_script::{
    ast::{ConnectStmt, DeployEndpoint},
    srs::{ConnectorDecl, DeployFlow, Query},
};

#[derive(Debug, PartialEq, PartialOrd, Eq, Hash, Clone)]
pub(crate) struct DeploymentId(pub String);

impl From<&DeployFlow> for DeploymentId {
    fn from(f: &DeployFlow) -> Self {
        DeploymentId(f.instance_id.id().to_string())
    }
}

#[derive(Debug, PartialEq, PartialOrd, Eq, Hash)]
pub(crate) struct ConnectorId(String);

impl From<&DeployEndpoint> for ConnectorId {
    fn from(e: &DeployEndpoint) -> Self {
        ConnectorId(e.instance().to_string())
    }
}

impl From<&ConnectorDecl> for ConnectorId {
    fn from(e: &ConnectorDecl) -> Self {
        ConnectorId(e.instance_id.clone())
    }
}

impl Borrow<str> for ConnectorId {
    fn borrow(&self) -> &str {
        &self.0
    }
}
#[derive(Debug, PartialEq, PartialOrd, Eq, Hash)]
pub(crate) struct PipelineId(pub String);
impl From<&DeployEndpoint> for PipelineId {
    fn from(e: &DeployEndpoint) -> Self {
        PipelineId(e.instance().to_string())
    }
}

impl From<&Query> for PipelineId {
    fn from(e: &Query) -> Self {
        PipelineId(e.instance_id.clone())
    }
}

impl Borrow<str> for PipelineId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

#[allow(dead_code)] // FIXME
#[derive(Debug)]
/// Control Plane message accepted by each binding control plane handler
pub enum Msg {
    /// start all contained instances
    Start,
    /// pause all contained instances
    Pause,
    /// resume all contained instance from pause
    Resume,
    /// stop all contained instances, and get notified via the given sender when the stop process is done
    Stop(async_std::channel::Sender<Result<()>>),
    /// drain all contained instances, and get notified via the given sender when the drain process is done
    Drain(async_std::channel::Sender<Result<()>>),
    /// request a `StatusReport` from this instance
    Report(async_std::channel::Sender<StatusReport>),
}
type Addr = async_std::channel::Sender<Msg>;

#[derive(Debug)]
pub(crate) struct Deployment {
    links: Vec<ConnectStmt>,
}

fn second<T1, T2>(t: &(T1, T2)) -> &T2 {
    &t.1
}

/// Status Report for a Binding
pub struct StatusReport {
    /// the url of the instance this report describes
    pub url: TremorUrl,
    /// the current state
    pub status: InstanceState,
}
impl Deployment {
    async fn link(
        connectors: &HashMap<ConnectorId, (TremorUrl, connectors::Addr)>,
        pipelines: &HashMap<PipelineId, (TremorUrl, pipeline::Addr)>,
        link: &ConnectStmt,
    ) -> Result<()> {
        match link {
            ConnectStmt::ConnectorToPipeline { from, to, .. } => {
                let (_, connector) = connectors
                    .get(from.instance())
                    .ok_or(format!("FIXME: connector {} not found", from.artefact()))?;

                let (output_url, pipeline) = pipelines
                    .get(to.instance())
                    .ok_or(format!("FIXME: pipeline {} not found", to.artefact()))?
                    .clone();

                // this is some odd stuff to have here
                let timeout = Duration::from_secs(2);

                let (tx, rx) = bounded(1);

                let msg = connectors::Msg::Link {
                    port: from.port().to_string().into(),
                    pipelines: vec![(output_url.with_port(to.port()), pipeline)],
                    result_tx: tx.clone(),
                };
                connector.send(msg).await.map_err(|e| -> Error {
                    format!("Could not send to connector: {}", e).into()
                })?;
                rx.recv().timeout(timeout).await???;
                // FIXME: move the connecto message from connector to pipeline here
            }
            ConnectStmt::PipelineToConnector { from, to, .. } => {
                let (input_url, pipeline) = pipelines
                    .get(from.instance())
                    .ok_or(format!("FIXME: pipeline {} not found", from.artefact()))?;

                let (output_url, connector) = connectors
                    .get(to.instance())
                    .ok_or(format!("FIXME: connector {} not found", to.artefact()))?
                    .clone();

                // first link the pipeline to the connector
                let msg = crate::pipeline::MgmtMsg::ConnectOutput {
                    port: from.port().to_string().into(),
                    output_url: output_url.with_port(to.port()),
                    target: connector.clone().try_into()?,
                };
                pipeline.send_mgmt(msg).await.map_err(|e| -> Error {
                    format!("Could not send to pipeline: {}", e).into()
                })?;

                // then link the connector to the pipeline
                // this is some odd stuff to have here
                let timeout = Duration::from_secs(2);

                let (tx, rx) = bounded(1);

                let msg = connectors::Msg::Link {
                    port: to.port().to_string().into(),
                    pipelines: vec![(input_url.clone().with_port(from.port()), pipeline.clone())],
                    result_tx: tx.clone(),
                };
                connector.send(msg).await.map_err(|e| -> Error {
                    format!("Could not send to connector: {}", e).into()
                })?;
                rx.recv().timeout(timeout).await???;
            }
            ConnectStmt::PipelineToPipeline { from, to, .. } => {
                let (_, from_pipeline) = pipelines
                    .get(from.instance())
                    .ok_or(format!("FIXME: pipeline {} not found", from.artefact()))?;
                let (output_url, to_pipeline) = pipelines
                    .get(to.instance())
                    .ok_or(format!("FIXME: pipeline {} not found", from.artefact()))?;
                let msg = crate::pipeline::MgmtMsg::ConnectOutput {
                    port: from.port().to_string().into(),
                    output_url: output_url.clone().with_port(to.port()),
                    target: to_pipeline.clone().into(),
                };
                from_pipeline.send_mgmt(msg).await.map_err(|e| -> Error {
                    format!("Could not send to pipeline: {}", e).into()
                })?;
            }
        }
        Ok(())
    }

    pub(crate) async fn start(
        src: String,
        flow: DeployFlow,
        oidgen: &mut OperatorIdGen,
        cidgen: &mut ConnectorIdGen,
        known_connectors: &KnownConnectors,
    ) -> Result<Self> {
        let mut pipelines = HashMap::new();
        let mut connectors = HashMap::new();

        for decl in &flow.decl.connectors {
            let url = TremorUrl::from_connector_instance(decl.artefact_id.id(), &decl.instance_id);

            let connector = crate::Connector::from_decl(decl)?;
            // FIXME
            connectors.insert(
                ConnectorId::from(decl),
                connectors::spawn(url, cidgen, known_connectors, connector).await?,
            );
        }
        for decl in &flow.decl.pipelines {
            let url = TremorUrl::parse(&format!(
                "/pipeline/{}/{}",
                decl.artifact_id.clone(),
                decl.instance_id
            ))?;
            let pipeline =
                tremor_pipeline::query::Query(tremor_script::Query::from_troy(&src, decl)?);
            let addr = pipeline::spawn(url, pipeline, oidgen).await?;
            pipelines.insert(PipelineId::from(decl), addr);
        }

        // link all the instances
        for link in &flow.decl.links {
            Deployment::link(&connectors, &pipelines, link).await?;
        }

        let addr = Deployment::spawn_task(pipelines, connectors, &flow.decl.links).await?;

        addr.send(Msg::Start).await?;

        let this = Deployment {
            links: flow.decl.links.clone(),
        };

        Ok(this)
    }

    /// task handling each binding instance control plane
    #[allow(clippy::too_many_lines)]
    async fn spawn_task(
        pipelines: HashMap<PipelineId, (TremorUrl, pipeline::Addr)>,
        connectors: HashMap<ConnectorId, (TremorUrl, connectors::Addr)>,
        links: &[ConnectStmt],
    ) -> Result<Addr> {
        let url = TremorUrl::parse("tremor://localhost/binding/FIXME/FIXME/FIXME")
            .expect("FIXME! really, please fix meeeee!"); // FIXME: figure out identification and url scheme for bindings
        let (msg_tx, msg_rx) = bounded(crate::QSIZE.load(Ordering::Relaxed));
        let (drain_tx, drain_rx) = unbounded();
        let (stop_tx, stop_rx) = unbounded();

        #[derive(Debug)]
        /// wrapper for all possible messages handled by the binding task
        enum MsgWrapper {
            Msg(Msg),
            DrainResult(ConnectorResult<()>),
            StopResult(ConnectorResult<()>),
        }

        let mut input_channel = PriorityMerge::new(
            msg_rx.map(MsgWrapper::Msg),
            PriorityMerge::new(
                drain_rx.map(MsgWrapper::DrainResult),
                stop_rx.map(MsgWrapper::StopResult),
            ),
        );
        let addr = msg_tx;
        let mut state = InstanceState::Initialized;
        // let registries = self.reg.clone();

        // extracting connectors and pipes from the links
        let sink_connectors: HashSet<ConnectorId> = links
            .iter()
            .filter_map(|c| {
                if let ConnectStmt::PipelineToConnector { to, .. } = c {
                    Some(ConnectorId::from(to))
                } else {
                    None
                }
            })
            .collect();
        let source_connectors: HashSet<ConnectorId> = links
            .iter()
            .filter_map(|c| {
                if let ConnectStmt::ConnectorToPipeline { from, .. } = c {
                    Some(ConnectorId::from(from))
                } else {
                    None
                }
            })
            .collect();

        let pipelines: Vec<_> = pipelines.values().map(second).cloned().collect();

        let start_points: Vec<_> = source_connectors
            .difference(&sink_connectors)
            .map(|p| connectors.get(p).map(second).unwrap())
            .cloned()
            .collect();
        let mixed_pickles: Vec<_> = sink_connectors
            .intersection(&source_connectors)
            .map(|p| connectors.get(p).map(second).unwrap())
            .cloned()
            .collect();
        let end_points: Vec<_> = sink_connectors
            .difference(&source_connectors)
            .map(|p| connectors.get(p).map(second).unwrap())
            .cloned()
            .collect();

        // for receiving drain/stop completion notifications from connectors
        let mut expected_drains: usize = 0;
        let mut expected_stops: usize = 0;

        // for storing senders that have been sent to us
        let mut drain_senders = Vec::with_capacity(1);
        let mut stop_senders = Vec::with_capacity(1);

        async fn wait_for_responses(
            rx: Receiver<ConnectorResult<()>>,
            n: usize,
        ) -> Result<Vec<()>> {
            futures::future::join_all(std::iter::repeat_with(|| rx.recv()).take(n))
                .await
                .into_iter()
                .map(|r| r.map_err(Error::from))
                .map(|res| res.and_then(|r| r.res))
                .collect::<Result<Vec<()>>>()
        }

        task::spawn::<_, Result<()>>(async move {
            while let Some(wrapped) = input_channel.next().await {
                match wrapped {
                    MsgWrapper::Msg(Msg::Start) if state == InstanceState::Initialized => {
                        // start all pipelines first - order doesnt matter as connectors aren't started yet
                        for pipe in &pipelines {
                            pipe.start().await?;
                        }
                        let (tx, rx) = unbounded();
                        // start sink connectors first
                        for conn in &end_points {
                            conn.start(tx.clone()).await?;
                        }
                        wait_for_responses(rx.clone(), end_points.len()).await?;

                        // start source/sink connectors in random order
                        for conn in &mixed_pickles {
                            conn.start(tx.clone()).await?;
                        }
                        wait_for_responses(rx.clone(), mixed_pickles.len()).await?;

                        // wait for mixed pickles to be connected
                        // start source only connectors
                        for conn in &start_points {
                            conn.start(tx.clone()).await?;
                        }
                        wait_for_responses(rx.clone(), start_points.len()).await?;

                        state = InstanceState::Running;
                    }
                    MsgWrapper::Msg(Msg::Start) => {
                        info!(
                            "[Flow::{}] Ignoring Start message. Current state: {}",
                            &url, &state
                        );
                    }
                    MsgWrapper::Msg(Msg::Pause) if state == InstanceState::Running => {
                        info!("[Flow::{}] Pausing...", &url);
                        for source in &start_points {
                            source.pause().await?;
                        }
                        for source_n_sink in &mixed_pickles {
                            source_n_sink.pause().await?;
                        }
                        for sink in &end_points {
                            sink.pause().await?;
                        }
                        for pipeline in &pipelines {
                            pipeline.pause().await?;
                        }
                    }
                    MsgWrapper::Msg(Msg::Pause) => {
                        info!(
                            "[Flow::{}] Ignoring Pause message. Current state: {}",
                            &url, &state
                        );
                    }
                    MsgWrapper::Msg(Msg::Resume) if state == InstanceState::Paused => {
                        info!("[Flow::{}] Resuming...", &url);

                        for pipeline in &pipelines {
                            pipeline.resume().await?;
                        }

                        for sink in &end_points {
                            sink.resume().await?;
                        }
                        for source_n_sink in &mixed_pickles {
                            source_n_sink.resume().await?;
                        }
                        for source in &start_points {
                            source.resume().await?;
                        }
                    }
                    MsgWrapper::Msg(Msg::Resume) => {
                        info!(
                            "[Flow::{}] Ignoring Resume message. Current state: {}",
                            &url, &state
                        );
                    }
                    MsgWrapper::Msg(Msg::Drain(_sender)) if state == InstanceState::Draining => {
                        info!(
                            "[Flow::{}] Ignoring Drain message. Current state: {}",
                            &url, &state
                        );
                    }
                    MsgWrapper::Msg(Msg::Drain(sender)) => {
                        info!("[Flow::{}] Draining...", &url);
                        drain_senders.push(sender);

                        // QUIESCENCE
                        // - send drain msg to all connectors
                        // - wait until
                        //   a) all connectors are drained (means all pipelines in between are also drained) or
                        //   b) we timed out

                        // source only connectors
                        for start_point in &start_points {
                            if let Err(e) = start_point.drain(drain_tx.clone()).await {
                                error!(
                                    "[Flow::{}] Error starting Draining Connector {:?}: {}",
                                    &url, start_point, e
                                );
                            } else {
                                expected_drains += 1;
                            }
                        }
                        // source/sink connectors
                        for mixed_pickle in &mixed_pickles {
                            if let Err(e) = mixed_pickle.drain(drain_tx.clone()).await {
                                error!(
                                    "[Flow::{}] Error starting Draining Connector {:?}: {}",
                                    &url, mixed_pickle, e
                                );
                            } else {
                                expected_drains += 1;
                            }
                        }
                        // sink only connectors
                        for end_point in &end_points {
                            if let Err(e) = end_point.drain(drain_tx.clone()).await {
                                error!(
                                    "[Flow::{}] Error starting Draining Connector {:?}: {}",
                                    &url, end_point, e
                                );
                            } else {
                                expected_drains += 1;
                            }
                        }
                    }
                    MsgWrapper::Msg(Msg::Stop(sender)) => {
                        info!("[Flow::{}] Stopping...", &url);
                        stop_senders.push(sender);

                        for connector in end_points
                            .iter()
                            .chain(start_points.iter())
                            .chain(mixed_pickles.iter())
                        {
                            if let Err(e) = connector.stop(stop_tx.clone()).await {
                                error!(
                                    "[Flow::{}] Error stopping connector {:?}: {}",
                                    &url, connector, e
                                );
                            } else {
                                expected_stops += 1;
                            }
                        }
                        for pipeline in &pipelines {
                            if let Err(e) = pipeline.stop().await {
                                error!(
                                    "[Flow::{}] Error stopping pipeline {:?}: {}",
                                    &url, pipeline, e
                                );
                            }
                        }
                    }
                    MsgWrapper::Msg(Msg::Report(sender)) => {
                        // TODO: aggregate states of all containing instances
                        let report = StatusReport {
                            url: url.clone(),
                            status: state.clone(),
                        };
                        if let Err(e) = sender.send(report).await {
                            error!("[Flow::{}] Error sending status report: {}", &url, e);
                        }
                    }
                    MsgWrapper::DrainResult(conn_res) => {
                        info!("[Flow::{}] Connector {} drained.", &url, &conn_res.url);
                        if let Err(e) = conn_res.res {
                            error!(
                                "[Flow::{}] Error during Draining in Connector {}: {}",
                                &url, &conn_res.url, e
                            );
                        }
                        let old = expected_drains;
                        expected_drains = expected_drains.saturating_sub(1);
                        if expected_drains == 0 && old > 0 {
                            info!("[Flow::{}] All connectors are drained.", &url);
                            // upon last drain
                            for drain_sender in drain_senders.drain(..) {
                                if let Err(_) = drain_sender.send(Ok(())).await {
                                    error!(
                                        "[Flow::{}] Error sending successful Drain result",
                                        &url
                                    );
                                }
                            }
                        }
                    }
                    MsgWrapper::StopResult(conn_res) => {
                        info!("[Flow::{}] Connector {} stopped.", &url, &conn_res.url);
                        if let Err(e) = conn_res.res {
                            error!(
                                "[Flow::{}] Error during Draining in Connector {}: {}",
                                &url, &conn_res.url, e
                            );
                        }
                        let old = expected_stops;
                        expected_stops = expected_stops.saturating_sub(1);
                        if expected_stops == 0 && old > 0 {
                            info!("[Flow::{}] All connectors are stopped.", &url);
                            // upon last stop
                            for stop_sender in stop_senders.drain(..) {
                                if let Err(_) = stop_sender.send(Ok(())).await {
                                    error!("[Flow::{}] Error sending successful Stop result", &url);
                                }
                            }
                            break;
                        }
                    }
                }
            }
            info!("[Flow::{}] Binding Stopped.", &url);
            Ok(())
        });
        Ok(addr)
    }
}
