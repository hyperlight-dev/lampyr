# Lampyr

Lampyr, named after the firefly family Lampyridae, provides a developer-facing
model for applications that run close to users with fast, hardware-isolated
activation.

## Language

**Lampyr Service**:
A named application that Lampyr builds, deploys, routes to, and activates at
the edge.
_Avoid_: Edge Service, function, Kubernetes Service

**Service Handler**:
Customer-owned code that defines a Lampyr Service's application behavior
without owning its protocol server or lifecycle.
_Avoid_: Server, function

**Runtime Adapter**:
The service-local protocol boundary that translates requests, events, and
sessions into calls to a Service Handler.
_Avoid_: Handler, application server

**Service Template**:
A maintained definition from which Lampyr creates or upgrades the customer-owned
and generated source of a Lampyr Service.
_Avoid_: Example, sample

**Application Model**:
The choice of whether Lampyr invokes customer application logic through a
Runtime Adapter or the customer application owns its listening process.

**Handler Application**:
An Application Model in which a Runtime Adapter invokes a customer-owned
Service Handler.
_Avoid_: Function

**Standalone Application**:
An Application Model in which the customer owns the complete listening process
while Lampyr retains the Service lifecycle and isolation boundary.
_Avoid_: Bring Your Own Container, Kubernetes workload

**Fiber**:
An individual isolated execution of a Lampyr Service.

**Lampyr Service Revision**:
An immutable version of a Lampyr Service's application and execution
configuration that can receive traffic.
_Avoid_: Edge Revision

**Capacity Grant**:
An authorization to run a bounded number of fibers of a Lampyr Service Revision
within specified resource and lease limits.

**Home**:
The execution location assigned to a Capacity Grant that manages its authorized
fibers.

**Execution Policy**:
The choice of whether a Lampyr Service dedicates a fiber to one request or
allows a fiber to serve multiple requests.

**Fresh-per-request**:
An execution policy in which a fiber serves one request and then ends.

**Reuse**:
An execution policy in which a fiber may serve multiple requests before it
ends. Reuse does not imply durable state or a session belonging to a user.

**Service Binding**:
A named dependency that Lampyr resolves for an environment and provides to a
Lampyr Service as configuration. A Service Binding can derive the connectivity
it requires, but raw network permission does not create a Service Binding.
_Avoid_: Outbound host, connection

**Outbound Allowance**:
Permission for application code to reach a destination whose address Lampyr
does not provide.
_Avoid_: Binding

**Event Source**:
A provider-backed ingress and routing integration that delivers selected events to
a Lampyr Service.

**Cluster Provider**:
A provider role that creates, connects to, observes, and deletes cluster
infrastructure.

**Provider Kubernetes Plan**:
Kubernetes resources supplied by a Cluster Provider for Lampyr to apply before
or after the shared platform, or during retained-cluster cleanup. Approving the
plan trusts the provider as a Kubernetes cluster operator.

**Event Source Provider**:
A provider role that defines and manages one kind of Event Source. A provider,
such as Azure, may implement both Cluster Provider and Event Source Provider
roles.

**Workspace**:
An optional customer-owned collection of Lampyr Services, editable Kubernetes
manifests, and named infrastructure sources. A Lampyr Service can also exist
independently.

**Infrastructure Source**:
Customer-owned, provider-specific desired state for one named infrastructure
installation.

**External Exposure**:
An explicit declaration that a Lampyr Service can receive requests from outside
its Workspace or cluster. Internal service calls do not require external exposure.
_Avoid_: Ingress Service

**Lampyr Home**:
Private machine-local support data used by Lampyr. It is not customer source
and is not part of a Workspace.
