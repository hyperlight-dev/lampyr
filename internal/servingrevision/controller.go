// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package servingrevision

import (
	"context"
	"crypto/sha256"
	"crypto/x509"
	"encoding/json"
	"encoding/pem"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/Azure/fiberd/pkg/tlsconf"
	"github.com/google/go-containerregistry/pkg/name"
	"github.com/hyperlight-dev/lampyr/internal/project"
	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/apimachinery/pkg/util/intstr"
	kvalidation "k8s.io/apimachinery/pkg/util/validation"
	"k8s.io/client-go/dynamic"
	"k8s.io/client-go/kubernetes"
	"k8s.io/client-go/util/retry"
	"knative.dev/networking/pkg/apis/networking"
	"knative.dev/pkg/client/injection/kube/client"
	"knative.dev/pkg/configmap"
	"knative.dev/pkg/controller"
	dynamicclient "knative.dev/pkg/injection/clients/dynamicclient"
	"knative.dev/pkg/kmap"
	"knative.dev/pkg/reconciler"
	"knative.dev/serving/pkg/apis/autoscaling"
	servingv1 "knative.dev/serving/pkg/apis/serving/v1"
	revisioninformer "knative.dev/serving/pkg/client/injection/informers/serving/v1/revision"
	revisionreconciler "knative.dev/serving/pkg/client/injection/reconciler/serving/v1/revision"
)

const (
	controllerAgentName = "lampyr-revision-controller"
	systemNamespace     = "lampyr-system"
	routerService       = "fiber-router.lampyr-system.svc.cluster.local"
	routerH2CService    = "fiber-router-h2c.lampyr-system.svc.cluster.local"
	routerReadyURL      = "http://" + routerService + "/readyz"
	defaultMaxFibers    = 100
	templateAnnotation  = "lampyr.dev/fiberd-template"
	grantSpecAnnotation = "lampyr.dev/grant-spec"
	// sealKeySecret holds the fleet seal key that opens the sealed
	// templates ly deploy pushes; ly creates it beside the grant Pods.
	sealKeySecret = "lampyr-fiberd-seal-key"
	// routerCertSecret is where cert-manager keeps the fiber-router's
	// client certificate, whose key every grant is bound to.
	routerCertSecret = "fiber-router-tls"
	// maxGrantName is fiberd-k8s-controller's limit, so that the
	// fiberd-<name> Service it creates fits a DNS label.
	maxGrantName = 56
	// grantDeploymentPrefix names the Deployment fiberd-k8s-controller
	// runs a grant's Pods from: fiberd-<grant>.
	grantDeploymentPrefix = "fiberd-"
	// defaultGrantCeiling is the memory.high fiberd sets on all of a grant's
	// Fibers together, for up to defaultMaxFibers of them.
	defaultGrantCeiling = int64(1536) << 20
	// perFiberHomeMemory sizes the ceiling per allowed Fiber above
	// defaultMaxFibers. A busy Node.js Fiber measured about 12-13 MiB of RSS.
	perFiberHomeMemory = int64(16) << 20
	// maxGrantCeiling caps the ceiling, whatever max-scale asks for.
	maxGrantCeiling = int64(64) << 30
	// homeMemoryHeadroom is the Pod limit above the ceiling, for fiberd.
	homeMemoryHeadroom = int64(512) << 20
	// defaultHomeCPU is a grant Pod's CPU request up to defaultMaxFibers,
	// and maxHomeCPU caps the request above it (homeCPURequest).
	defaultHomeCPU = "10m"
	maxHomeCPU     = 2000
	readyResync    = 30 * time.Second
)

var capacityGrantResource = schema.GroupVersionResource{
	Group: "fiberd.io", Version: "v1alpha1", Resource: "capacitygrants",
}

type Reconciler struct {
	kube    kubernetes.Interface
	dynamic dynamic.Interface
	impl    *controller.Impl
	http    *http.Client
	router  string
}

func NewController(ctx context.Context, _ configmap.Watcher) *controller.Impl {
	revisionInformer := revisioninformer.Get(ctx)
	r := &Reconciler{
		kube:    client.Get(ctx),
		dynamic: dynamicclient.Get(ctx),
		http:    http.DefaultClient,
		router:  routerReadyURL,
	}
	impl := revisionreconciler.NewImpl(ctx, r, func(*controller.Impl) controller.Options {
		return controller.Options{AgentName: controllerAgentName}
	})
	r.impl = impl
	revisionInformer.Informer().AddEventHandler(controller.HandleAll(impl.Enqueue))
	return impl
}

func (r *Reconciler) ReconcileKind(ctx context.Context, revision *servingv1.Revision) reconciler.Event {
	revision.Status.InitializeConditions()
	spec, err := validateRevision(revision)
	if err != nil {
		revision.Status.MarkResourcesAvailableFalse("UnsupportedRevision", err.Error())
		revision.Status.MarkContainerHealthyFalse("UnsupportedRevision", err.Error())
		revision.Status.MarkActiveFalse("UnsupportedRevision", err.Error())
		return nil
	}

	if err := r.reconcileBackendService(ctx, revision, spec.protocol); err != nil {
		revision.Status.MarkResourcesAvailableFalse("BackendServiceFailed", err.Error())
		return fmt.Errorf("reconcile Revision backend Service: %w", err)
	}

	home, err := r.homeConfiguration(ctx)
	if err != nil {
		revision.Status.MarkResourcesAvailableUnknown("InstallationNotReady", err.Error())
		r.enqueueAfter(revision, time.Second)
		return nil
	}
	grant, err := desiredCapacityGrant(revision, spec, home)
	if err != nil {
		revision.Status.MarkResourcesAvailableFalse("UnsupportedRevision", err.Error())
		return nil
	}
	grants := r.dynamic.Resource(capacityGrantResource).Namespace(revision.Namespace)
	current, err := grants.Get(ctx, revision.Name, metav1.GetOptions{})
	switch {
	case apierrors.IsNotFound(err):
		if _, err := grants.Create(ctx, grant, metav1.CreateOptions{}); err != nil {
			revision.Status.MarkResourcesAvailableFalse("CapacityGrantCreateFailed", err.Error())
			return fmt.Errorf("create CapacityGrant: %w", err)
		}
		revision.Status.MarkResourcesAvailableUnknown("CapacityGrantPending", "CapacityGrant is being admitted")
		revision.Status.MarkContainerHealthyUnknown("ArtifactPending", "Fiberd is preparing the snapshot artifact")
		revision.Status.MarkActiveUnknown("CapacityPending", "Fiber capacity is not ready")
		r.enqueueAfter(revision, time.Second)
		return nil
	case err != nil:
		revision.Status.MarkResourcesAvailableUnknown("CapacityGrantReadFailed", err.Error())
		return fmt.Errorf("read CapacityGrant: %w", err)
	}

	if err := ensureGrantIdentity(current, grant); err != nil {
		revision.Status.MarkResourcesAvailableFalse("ImmutableGrantConflict", err.Error())
		return nil
	}
	// The controller rolls a new grant Pod for any spec change, so Lampyr
	// only replaces the spec it wrote and leaves the rollout to it.
	if current.GetAnnotations()[grantSpecAnnotation] != grant.GetAnnotations()[grantSpecAnnotation] {
		updated := current.DeepCopy()
		updated.Object["spec"] = grant.Object["spec"]
		annotations := updated.GetAnnotations()
		if annotations == nil {
			annotations = map[string]string{}
		}
		annotations[grantSpecAnnotation] = grant.GetAnnotations()[grantSpecAnnotation]
		updated.SetAnnotations(annotations)
		if _, err := grants.Update(ctx, updated, metav1.UpdateOptions{}); err != nil {
			revision.Status.MarkResourcesAvailableUnknown("CapacityGrantUpdateFailed", err.Error())
			return fmt.Errorf("update CapacityGrant: %w", err)
		}
		revision.Status.MarkResourcesAvailableUnknown("CapacityGrantUpdating", "CapacityGrant is rolling out")
		revision.Status.MarkActiveUnknown("CapacityPending", "Fiber capacity is being renewed")
		r.enqueueAfter(revision, time.Second)
		return nil
	}
	deployment, err := r.kube.AppsV1().Deployments(revision.Namespace).Get(ctx, grantDeploymentPrefix+revision.Name, metav1.GetOptions{})
	switch {
	case apierrors.IsNotFound(err):
		deployment = nil
	case err != nil:
		revision.Status.MarkResourcesAvailableUnknown("CapacityGrantReadFailed", err.Error())
		return fmt.Errorf("read CapacityGrant Deployment: %w", err)
	}
	ready, failed, message := capacityGrantReadiness(current, deployment)
	if failed {
		revision.Status.MarkResourcesAvailableFalse("CapacityGrantFailed", message)
		revision.Status.MarkActiveFalse("CapacityGrantFailed", message)
		r.enqueueAfter(revision, readyResync)
		return nil
	}
	if !ready {
		revision.Status.MarkResourcesAvailableUnknown("CapacityGrantPending", message)
		revision.Status.MarkContainerHealthyUnknown("ArtifactPending", message)
		revision.Status.MarkActiveUnknown("CapacityPending", message)
		r.enqueueAfter(revision, time.Second)
		return nil
	}
	version, err := r.publishRevision(ctx, revision, spec)
	if err != nil {
		revision.Status.MarkResourcesAvailableUnknown("RouterMetadataPending", err.Error())
		r.enqueueAfter(revision, time.Second)
		return nil
	}
	if err := r.waitForRouter(ctx, revision, version); err != nil {
		revision.Status.MarkResourcesAvailableUnknown("RouterMetadataPending", err.Error())
		r.enqueueAfter(revision, time.Second)
		return nil
	}

	revision.Status.ContainerStatuses = []servingv1.ContainerStatus{{
		Name:        spec.container.Name,
		ImageDigest: spec.containerImage,
	}}
	revision.Status.MarkResourcesAvailableTrue()
	revision.Status.MarkContainerHealthyTrue()
	revision.Status.MarkActiveTrue()
	r.enqueueAfter(revision, readyResync)
	return nil
}

// enqueueAfter reconciles revision again after delay. A Reconciler built
// without a controller, as in tests, only reconciles when it is called.
func (r *Reconciler) enqueueAfter(revision *servingv1.Revision, delay time.Duration) {
	if r.impl != nil {
		r.impl.EnqueueAfter(revision, delay)
	}
}

// capacityGrantReadiness reads the controller's conditions for the spec
// Lampyr wrote last. Ready alone is not enough: grant Pods hold the
// hypervisor device, so the controller replaces them with the Recreate
// strategy, and a Ready reported before the old Pod went describes that
// Pod, which serves the old spec, such as an old caller thumbprint. The
// new ReplicaSet must be available too. A rollout the Deployment gave up
// on is a failure, not a wait.
//
// The conditions alone can still be stale. The controller computes a
// pass's status from the Deployment as it found it before that pass
// applied the spec, so the pass that first observes a new spec reports the
// old Pod's finished rollout under the new generation. The controller
// patches that status only after the apply, and deployment is read after
// grant, so deployment already holds the new template: its own rollout,
// read as kubectl rollout status does, says whether the new Pod serves.
func capacityGrantReadiness(grant *unstructured.Unstructured, deployment *appsv1.Deployment) (ready, failed bool, message string) {
	observed, _, _ := unstructured.NestedInt64(grant.Object, "status", "observedGeneration")
	if observed != grant.GetGeneration() {
		return false, false, "CapacityGrant status is not current"
	}
	conditions, _, _ := unstructured.NestedSlice(grant.Object, "status", "conditions")
	found := map[string]map[string]any{}
	for _, value := range conditions {
		if condition, ok := value.(map[string]any); ok {
			if kind, ok := condition["type"].(string); ok {
				found[kind] = condition
			}
		}
	}
	describe := func(condition map[string]any) string {
		if message, _ := condition["message"].(string); message != "" {
			return message
		}
		reason, _ := condition["reason"].(string)
		return reason
	}
	progressing := found["Progressing"]
	if progressing["status"] == "False" {
		return false, true, "CapacityGrant rollout failed: " + describe(progressing)
	}
	if found["Ready"]["status"] != "True" {
		// These reasons block the controller from applying the spec, and no
		// rollout ends them: InvalidSpec lasts until the spec changes, and
		// Forbidden and the conflicts until a RoleBinding is added or
		// another owner's object goes, which the controller looks for
		// again every minute. So they are failures, not waits, though the
		// Revision is read again every readyResync and recovers once the
		// block clears.
		switch found["Ready"]["reason"] {
		case "InvalidSpec", "Forbidden", "DeploymentConflict", "ServiceConflict":
			return false, true, "CapacityGrant is blocked: " + describe(found["Ready"])
		}
		return false, false, "CapacityGrant is not ready: " + describe(found["Ready"])
	}
	if progressing["reason"] != "NewReplicaSetAvailable" {
		return false, false, "CapacityGrant is rolling out: " + describe(progressing)
	}
	if deployment == nil {
		return false, false, "CapacityGrant Deployment does not exist yet"
	}
	if !deploymentRolledOut(deployment) {
		return false, false, "CapacityGrant Deployment is rolling out its current template"
	}
	return true, false, ""
}

// deploymentRolledOut reports whether deployment runs its current template
// on every replica and keeps no Pod of an older one.
func deploymentRolledOut(deployment *appsv1.Deployment) bool {
	replicas := int32(1)
	if deployment.Spec.Replicas != nil {
		replicas = *deployment.Spec.Replicas
	}
	status := deployment.Status
	return status.ObservedGeneration == deployment.Generation &&
		status.UpdatedReplicas == replicas &&
		status.Replicas == replicas &&
		status.AvailableReplicas >= replicas
}

type revisionSpec struct {
	container      corev1.Container
	containerImage string
	template       name.Digest
	protocol       networking.ProtocolType
	maxFibers      int64
	memory         string
	execution      project.ExecutionPolicy
	timeout        time.Duration
}

func validateRevision(revision *servingv1.Revision) (revisionSpec, error) {
	if revision.Namespace != "default" {
		return revisionSpec{}, fmt.Errorf("Lampyr currently supports Knative Revisions only in namespace default")
	}
	if len(revision.Spec.Containers) != 1 {
		return revisionSpec{}, fmt.Errorf("exactly one container is required")
	}
	if len(revision.Spec.InitContainers) != 0 || len(revision.Spec.Volumes) != 0 {
		return revisionSpec{}, fmt.Errorf("init containers and volumes are not supported")
	}
	container := revision.Spec.Containers[0]
	if revision.Spec.ServiceAccountName != "" ||
		revision.Spec.RuntimeClassName != nil ||
		revision.Spec.SecurityContext != nil ||
		container.SecurityContext != nil {
		return revisionSpec{}, fmt.Errorf("service-account, runtime-class and security-context overrides are not supported")
	}
	if len(container.Command) != 0 ||
		len(container.Args) != 0 ||
		len(container.Env) != 0 ||
		len(container.EnvFrom) != 0 ||
		container.WorkingDir != "" {
		return revisionSpec{}, fmt.Errorf("container command, args, environment and working-directory overrides are not supported")
	}
	if container.LivenessProbe != nil ||
		container.StartupProbe != nil ||
		!isDefaultReadinessProbe(container.ReadinessProbe) {
		return revisionSpec{}, fmt.Errorf("custom readiness, liveness and startup probes are not supported")
	}
	responseStartTimeout := revision.Spec.ResponseStartTimeoutSeconds != nil &&
		*revision.Spec.ResponseStartTimeoutSeconds != 0
	idleTimeout := revision.Spec.IdleTimeoutSeconds != nil &&
		*revision.Spec.IdleTimeoutSeconds != 0
	if responseStartTimeout || idleTimeout {
		return revisionSpec{}, fmt.Errorf("response-start and idle timeout overrides are not supported")
	}
	for resourceName, quantity := range container.Resources.Requests {
		if resourceName != corev1.ResourceMemory && !quantity.IsZero() {
			return revisionSpec{}, fmt.Errorf("only a memory resource request is supported")
		}
	}
	for _, quantity := range container.Resources.Limits {
		if !quantity.IsZero() {
			return revisionSpec{}, fmt.Errorf("container resource limits are not supported")
		}
	}
	if _, err := name.NewDigest(container.Image, name.StrictValidation); err != nil {
		return revisionSpec{}, fmt.Errorf("container image must be immutable and digest-pinned: %w", err)
	}
	if len(revision.Name) > maxGrantName {
		return revisionSpec{}, fmt.Errorf("Revision names longer than %d characters cannot name a CapacityGrant", maxGrantName)
	}
	template, err := name.NewDigest(strings.TrimSpace(revision.Annotations[templateAnnotation]), name.StrictValidation)
	if err != nil {
		return revisionSpec{}, fmt.Errorf("%s must name a digest-pinned fiberd template; rebuild with ly build where the platform has its helper: %w", templateAnnotation, err)
	}
	if class := revision.Annotations[autoscaling.ClassAnnotationKey]; class != "" {
		return revisionSpec{}, fmt.Errorf("autoscaler class %q is not supported", class)
	}
	if metric := revision.Annotations[autoscaling.MetricAnnotationKey]; metric != "" {
		return revisionSpec{}, fmt.Errorf("autoscaling metric %q is not supported", metric)
	}
	minScale, _, err := scale(revision.Annotations, autoscaling.MinScaleAnnotation, 0)
	if err != nil {
		return revisionSpec{}, err
	}
	maxScale, maxScaleSet, err := scale(
		revision.Annotations,
		autoscaling.MaxScaleAnnotation,
		defaultMaxFibers,
	)
	if err != nil {
		return revisionSpec{}, err
	}
	if maxScaleSet && maxScale == 0 {
		return revisionSpec{}, fmt.Errorf("maxScale=0 (unlimited) is not supported")
	}
	if minScale != 0 {
		return revisionSpec{}, fmt.Errorf("minScale must be 0 until reserved Fiber capacity is supported")
	}
	concurrency := revision.Spec.GetContainerConcurrency()
	if concurrency != 1 {
		return revisionSpec{}, fmt.Errorf("containerConcurrency must be 1")
	}
	execution := project.ExecutionPolicy{
		Mode:        revision.Annotations["lampyr.dev/execution-mode"],
		IdleTimeout: revision.Annotations["lampyr.dev/idle-timeout"],
	}
	if execution.Mode == "" {
		execution.Mode = project.ExecutionFresh
	}
	effective, err := (project.Manifest{Execution: &execution}).EffectiveExecution()
	if err != nil {
		return revisionSpec{}, err
	}
	timeoutSeconds := int64(300)
	if revision.Spec.TimeoutSeconds != nil {
		timeoutSeconds = *revision.Spec.TimeoutSeconds
	}
	if timeoutSeconds <= 0 {
		return revisionSpec{}, fmt.Errorf("timeoutSeconds must be positive")
	}
	memory := "512Mi"
	if quantity := container.Resources.Requests.Memory(); quantity != nil && !quantity.IsZero() {
		memory = strconv.FormatInt(quantity.Value(), 10)
	}
	return revisionSpec{
		container:      container,
		containerImage: container.Image,
		template:       template,
		protocol:       revision.GetProtocol(),
		maxFibers:      maxScale,
		memory:         memory,
		execution:      effective,
		timeout:        time.Duration(timeoutSeconds) * time.Second,
	}, nil
}

func isDefaultReadinessProbe(probe *corev1.Probe) bool {
	if probe == nil {
		return true
	}
	return probe.TCPSocket != nil &&
		probe.TCPSocket.Port == intstr.FromInt(0) &&
		probe.HTTPGet == nil &&
		probe.Exec == nil &&
		probe.GRPC == nil &&
		probe.InitialDelaySeconds == 0 &&
		probe.TimeoutSeconds == 0 &&
		probe.PeriodSeconds == 0 &&
		probe.SuccessThreshold == 1 &&
		probe.FailureThreshold == 0 &&
		probe.TerminationGracePeriodSeconds == nil
}

func scale(
	annotations map[string]string,
	keys kmap.KeyPriority,
	fallback int64,
) (int64, bool, error) {
	key, value, found := keys.Get(annotations)
	if !found {
		return fallback, false, nil
	}
	parsed, err := strconv.ParseInt(value, 10, 32)
	if err != nil || parsed < 0 {
		return 0, true, fmt.Errorf("%s must be a non-negative integer", key)
	}
	return parsed, true, nil
}

type homePodMetadata struct {
	Labels      map[string]string
	Annotations map[string]string
}

// homeConfig is what every grant Pod of this installation shares.
type homeConfig struct {
	grantImage  string
	helperImage string
	// callerThumbprint names the router's key, the only caller a grant
	// Pod serves.
	callerThumbprint string
	metadata         homePodMetadata
	mirrors          map[string]registryMirror
}

type registryMirror struct {
	Registry  string `json:"registry"`
	PlainHTTP bool   `json:"plainHttp"`
}

func (r *Reconciler) homeConfiguration(ctx context.Context) (homeConfig, error) {
	installation, err := r.kube.CoreV1().ConfigMaps("kube-system").
		Get(ctx, "lampyr-install", metav1.GetOptions{})
	if err != nil {
		return homeConfig{}, fmt.Errorf("read Lampyr installation metadata: %w", err)
	}
	config := homeConfig{
		grantImage:  strings.TrimSpace(installation.Data["grantImage"]),
		helperImage: strings.TrimSpace(installation.Data["helperImage"]),
	}
	if config.grantImage == "" || config.helperImage == "" {
		return homeConfig{}, fmt.Errorf("Lampyr installation metadata has no grantImage or helperImage")
	}
	if config.metadata, err = parseHomePodMetadata(installation.Data["homePodMetadata"]); err != nil {
		return homeConfig{}, err
	}
	// cert-manager issues the router's certificate after install applies
	// it, so a missing one is waited for like the rest of the installation.
	certificate, err := r.kube.CoreV1().Secrets(systemNamespace).Get(ctx, routerCertSecret, metav1.GetOptions{})
	if err != nil {
		return homeConfig{}, fmt.Errorf("read the fiber-router's certificate: %w", err)
	}
	if config.callerThumbprint, err = certificateThumbprint(certificate.Data[corev1.TLSCertKey]); err != nil {
		return homeConfig{}, fmt.Errorf("the fiber-router's certificate: %w", err)
	}
	if raw := strings.TrimSpace(os.Getenv("FIBERD_REGISTRY_MIRRORS")); raw != "" {
		if err := json.Unmarshal([]byte(raw), &config.mirrors); err != nil {
			return homeConfig{}, fmt.Errorf("decode FIBERD_REGISTRY_MIRRORS: %w", err)
		}
	}
	return config, nil
}

// certificateThumbprint is fiberd's tlsconf.ThumbprintFile over PEM: the
// RFC 7638 thumbprint of the first certificate's public key, which fiberd
// compares with a grant's cnf jkt. A renewal over the same key keeps it.
func certificateThumbprint(data []byte) (string, error) {
	for {
		var block *pem.Block
		if block, data = pem.Decode(data); block == nil {
			return "", fmt.Errorf("holds no PEM certificate")
		}
		if block.Type != "CERTIFICATE" {
			continue
		}
		certificate, err := x509.ParseCertificate(block.Bytes)
		if err != nil {
			return "", err
		}
		return tlsconf.Thumbprint(certificate)
	}
}

// zygoteRegistry is where grant Pods pull the template from. ly deploy
// records the registry it pushed to, which for a local registry is an
// address only the host can reach, so a mirror maps it to the one the
// cluster reaches.
func zygoteRegistry(template name.Digest, mirrors map[string]registryMirror) (string, error) {
	repository := template.Context()
	registry := repository.RegistryStr()
	mirror, found := mirrors[registry]
	if !found {
		return repository.Name(), nil
	}
	if strings.TrimSpace(mirror.Registry) == "" {
		return "", fmt.Errorf("registry mirror for %s is empty", registry)
	}
	address := mirror.Registry + "/" + repository.RepositoryStr()
	if mirror.PlainHTTP {
		address = "http://" + address
	}
	return address, nil
}

func parseHomePodMetadata(raw string) (homePodMetadata, error) {
	metadata := homePodMetadata{
		Labels:      map[string]string{},
		Annotations: map[string]string{},
	}
	if strings.TrimSpace(raw) == "" {
		return metadata, nil
	}
	var document map[string]json.RawMessage
	if err := json.Unmarshal([]byte(raw), &document); err != nil {
		return homePodMetadata{}, fmt.Errorf("decode homePodMetadata: %w", err)
	}
	for field, encoded := range document {
		var entries map[string]string
		if field != "labels" && field != "annotations" {
			return homePodMetadata{}, fmt.Errorf("homePodMetadata supports only labels and annotations")
		}
		if err := json.Unmarshal(encoded, &entries); err != nil {
			return homePodMetadata{}, fmt.Errorf("decode homePodMetadata %s: %w", field, err)
		}
		for key, value := range entries {
			if errors := kvalidation.IsQualifiedName(key); len(errors) != 0 {
				return homePodMetadata{}, fmt.Errorf("invalid home Pod metadata key %q: %s", key, strings.Join(errors, ", "))
			}
			if len(value) > 4096 || strings.ContainsRune(value, '\x00') {
				return homePodMetadata{}, fmt.Errorf("invalid home Pod metadata value for %q", key)
			}
			if isControllerReservedKey(key) {
				return homePodMetadata{}, fmt.Errorf("homePodMetadata %s key %q is reserved by fiberd-k8s-controller, which would refuse every grant", field, key)
			}
			if field == "labels" {
				if errors := kvalidation.IsValidLabelValue(value); len(errors) != 0 {
					return homePodMetadata{}, fmt.Errorf("invalid home Pod label %q: %s", key, strings.Join(errors, ", "))
				}
				if isReservedHomeLabel(key) {
					return homePodMetadata{}, fmt.Errorf("homePodMetadata cannot override label %q", key)
				}
				metadata.Labels[key] = value
			} else {
				metadata.Annotations[key] = value
			}
		}
	}
	return metadata, nil
}

// controllerReservedKeyPrefixes are the grant Pod label and annotation key
// prefixes fiberd-k8s-controller refuses as InvalidSpec, from its
// podtemplate.reservedKeyPrefixes; it refuses keys under a fiberd.io
// subdomain too.
var controllerReservedKeyPrefixes = []string{
	"fiberd.io/",
	"container.apparmor.security.beta.kubernetes.io/",
	"seccomp.security.alpha.kubernetes.io/",
	"container.seccomp.security.alpha.kubernetes.io/",
	"k8s.v1.cni.cncf.io/",
	"cni.projectcalico.org/",
	"io.kubernetes.cri-o.",
	"dev.gvisor.",
	"io.katacontainers.",
}

func isControllerReservedKey(key string) bool {
	return strings.Contains(key, ".fiberd.io/") ||
		slices.ContainsFunc(controllerReservedKeyPrefixes, func(prefix string) bool { return strings.HasPrefix(key, prefix) })
}

func isReservedHomeLabel(key string) bool {
	switch key {
	case "app",
		"lampyr.dev/home",
		"edge.lampyr.dev/service",
		"serving.knative.dev/revision",
		"serving.knative.dev/service":
		return true
	default:
		return false
	}
}

func (r *Reconciler) reconcileBackendService(
	ctx context.Context,
	revision *servingv1.Revision,
	protocol networking.ProtocolType,
) error {
	portName := networking.ServicePortName(protocol)
	appProtocol := networking.AppProtocol(protocol)
	port := int32(networking.ServicePort(protocol))
	router := routerService
	if protocol == networking.ProtocolH2C {
		router = routerH2CService
	}
	desired := &corev1.Service{
		ObjectMeta: metav1.ObjectMeta{
			Name:      revision.Name,
			Namespace: revision.Namespace,
			Labels: map[string]string{
				"app.kubernetes.io/managed-by": "lampyr",
				"serving.knative.dev/revision": revision.Name,
			},
			OwnerReferences: []metav1.OwnerReference{*metav1.NewControllerRef(
				revision,
				servingv1.SchemeGroupVersion.WithKind("Revision"),
			)},
		},
		Spec: corev1.ServiceSpec{
			Type:         corev1.ServiceTypeExternalName,
			ExternalName: router,
			Ports: []corev1.ServicePort{{
				Name:        portName,
				AppProtocol: appProtocol,
				Port:        port,
				TargetPort:  intstr.FromInt(int(port)),
				Protocol:    corev1.ProtocolTCP,
			}},
		},
	}
	services := r.kube.CoreV1().Services(revision.Namespace)
	current, err := services.Get(ctx, revision.Name, metav1.GetOptions{})
	if apierrors.IsNotFound(err) {
		_, err = services.Create(ctx, desired, metav1.CreateOptions{})
		return err
	}
	if err != nil {
		return err
	}
	if !metav1.IsControlledBy(current, revision) {
		return fmt.Errorf("Service %s/%s is not controlled by the current Revision", revision.Namespace, revision.Name)
	}
	if current.Spec.Type != desired.Spec.Type ||
		current.Spec.ExternalName != desired.Spec.ExternalName ||
		len(current.Spec.Ports) != 1 ||
		current.Spec.Ports[0].Name != desired.Spec.Ports[0].Name ||
		current.Spec.Ports[0].Port != desired.Spec.Ports[0].Port {
		updated := current.DeepCopy()
		updated.Spec.Type = desired.Spec.Type
		updated.Spec.ExternalName = desired.Spec.ExternalName
		updated.Spec.Ports = desired.Spec.Ports
		_, err := services.Update(ctx, updated, metav1.UpdateOptions{})
		return err
	}
	return nil
}

func desiredCapacityGrant(
	revision *servingv1.Revision,
	spec revisionSpec,
	home homeConfig,
) (*unstructured.Unstructured, error) {
	registry, err := zygoteRegistry(spec.template, home.mirrors)
	if err != nil {
		return nil, err
	}
	labels := map[string]any{
		"app.kubernetes.io/managed-by":      "lampyr",
		"serving.knative.dev/revision":      revision.Name,
		"serving.knative.dev/service":       revision.Labels["serving.knative.dev/service"],
		"serving.knative.dev/configuration": revision.Labels["serving.knative.dev/configuration"],
	}
	podLabels := map[string]any{
		"lampyr.dev/home":              "true",
		"serving.knative.dev/revision": revision.Name,
		"serving.knative.dev/service":  revision.Labels["serving.knative.dev/service"],
	}
	for key, value := range home.metadata.Labels {
		podLabels[key] = value
	}
	podAnnotations := map[string]any{}
	for key, value := range home.metadata.Annotations {
		podAnnotations[key] = value
	}
	ceiling := homeMemoryCeiling(spec.maxFibers)
	limits := map[string]any{
		"memory":                    "2Gi",
		"ephemeral-storage":         "10Gi",
		"hyperlight.dev/hypervisor": int64(1),
	}
	// A grant of up to defaultMaxFibers keeps the default memory ceiling.
	if ceiling != defaultGrantCeiling {
		limits["memory"] = strconv.FormatInt(ceiling+homeMemoryHeadroom, 10)
	}
	grantSpec := map[string]any{
		"zygote": map[string]any{
			"digest":            spec.template.DigestStr(),
			"registry":          registry,
			"sealKeySecretName": sealKeySecret,
		},
		"fibers":           map[string]any{"max": spec.maxFibers},
		"writeBudget":      spec.memory,
		"callerThumbprint": home.callerThumbprint,
		"runtime":          "hyperlight",
		"helperImage":      home.helperImage,
		"podTemplate": map[string]any{
			"metadata": map[string]any{"labels": podLabels, "annotations": podAnnotations},
			"spec": map[string]any{
				"nodeSelector": map[string]any{
					"kubernetes.io/os":   "linux",
					"kubernetes.io/arch": "amd64",
				},
				"containers": []any{map[string]any{
					"name":  "fiberd",
					"image": home.grantImage,
					"args":  []any{"-grant-ceiling=" + strconv.FormatInt(ceiling, 10)},
					// The controller copies limits to requests, so these
					// are what the grant Pod reserves. The hypervisor
					// resource lands the Pod on a node with /dev/kvm. CPU
					// is a request alone, so the Pod is never throttled.
					"resources": map[string]any{
						"limits":   limits,
						"requests": map[string]any{"cpu": homeCPURequest(spec.maxFibers)},
					},
				}},
			},
		},
	}
	encoded, err := json.Marshal(grantSpec)
	if err != nil {
		return nil, err
	}
	owner, _ := json.Marshal(metav1.NewControllerRef(
		revision,
		servingv1.SchemeGroupVersion.WithKind("Revision"),
	))
	var ownerReference map[string]any
	_ = json.Unmarshal(owner, &ownerReference)
	return &unstructured.Unstructured{Object: map[string]any{
		"apiVersion": "fiberd.io/v1alpha1",
		"kind":       "CapacityGrant",
		"metadata": map[string]any{
			"name":      revision.Name,
			"namespace": revision.Namespace,
			"labels":    labels,
			"annotations": map[string]any{
				"lampyr.dev/container-image": spec.containerImage,
				grantSpecAnnotation:          fmt.Sprintf("sha256:%x", sha256.Sum256(encoded)),
			},
			"ownerReferences": []any{ownerReference},
		},
		"spec": grantSpec,
	}}, nil
}

// homeCPURequest is the CPU a grant Pod asks the scheduler for, as the
// old home did: 10m up to defaultMaxFibers, then a millicore per allowed
// Fiber, at most two CPUs, which keeps CPU for the Pod's own probes when
// many Fibers restore at once. It is a request with no limit, so the Pod
// may use every CPU the node has free: a cap throttles restores, which
// every cold request waits for, and a two-CPU cap made a burst of 1,000
// cold requests three times slower. The controller makes a grant Pod's
// memory limit its request, but leaves a CPU request without a limit
// alone (Azure/fiberd-k8s-controller#39).
func homeCPURequest(fibers int64) string {
	if fibers <= defaultMaxFibers {
		return defaultHomeCPU
	}
	return strconv.FormatInt(min(fibers, maxHomeCPU), 10) + "m"
}

// homeMemoryCeiling is the memory ceiling for a grant that may run fibers
// Fibers: 1.5 GiB up to defaultMaxFibers, then 16 MiB per Fiber, at most
// maxGrantCeiling. The controller makes the grant Pod's memory limit its
// request too, so the scheduler places it only where that memory is free.
func homeMemoryCeiling(fibers int64) int64 {
	if fibers <= defaultMaxFibers {
		return defaultGrantCeiling
	}
	return min(maxGrantCeiling, fibers*perFiberHomeMemory)
}

func ensureGrantIdentity(
	current *unstructured.Unstructured,
	desired *unstructured.Unstructured,
) error {
	currentOwner := metav1.GetControllerOf(current)
	desiredOwner := metav1.GetControllerOf(desired)
	if currentOwner == nil ||
		desiredOwner == nil ||
		currentOwner.APIVersion != desiredOwner.APIVersion ||
		currentOwner.Kind != desiredOwner.Kind ||
		currentOwner.Name != desiredOwner.Name ||
		currentOwner.UID != desiredOwner.UID {
		return fmt.Errorf("CapacityGrant is not controlled by the current Revision")
	}
	currentDigest, _, _ := unstructured.NestedString(current.Object, "spec", "zygote", "digest")
	desiredDigest, _, _ := unstructured.NestedString(desired.Object, "spec", "zygote", "digest")
	if currentDigest != desiredDigest {
		return fmt.Errorf("CapacityGrant already serves template %q, want %q", currentDigest, desiredDigest)
	}
	return nil
}

func revisionMetadataKey(revision *servingv1.Revision) string {
	return "revision-" + revision.Namespace + "--" + revision.Name + ".json"
}

func (r *Reconciler) publishRevision(
	ctx context.Context,
	revision *servingv1.Revision,
	spec revisionSpec,
) (string, error) {
	document, err := json.Marshal(struct {
		Kind      string                  `json:"kind"`
		Namespace string                  `json:"namespace"`
		Revision  string                  `json:"revision"`
		Execution project.ExecutionPolicy `json:"execution"`
		Timeout   string                  `json:"timeout"`
		Routed    bool                    `json:"routed"`
	}{
		Kind:      "revision",
		Namespace: revision.Namespace,
		Revision:  revision.Name,
		Execution: spec.execution,
		Timeout:   spec.timeout.String(),
		Routed:    revision.GetRoutingState() == servingv1.RoutingStateActive,
	})
	if err != nil {
		return "", err
	}
	version := fmt.Sprintf("sha256:%x", sha256.Sum256(document))
	configMaps := r.kube.CoreV1().ConfigMaps(systemNamespace)
	err = retry.RetryOnConflict(retry.DefaultRetry, func() error {
		config, err := configMaps.Get(ctx, "lampyr-routes", metav1.GetOptions{})
		if err != nil {
			return err
		}
		if config.Data == nil {
			config.Data = make(map[string]string)
		}
		key := revisionMetadataKey(revision)
		if config.Data[key] == string(document) {
			return nil
		}
		config.Data[key] = string(document)
		_, err = configMaps.Update(ctx, config, metav1.UpdateOptions{})
		return err
	})
	return version, err
}

func (r *Reconciler) waitForRouter(
	ctx context.Context,
	revision *servingv1.Revision,
	version string,
) error {
	query := url.Values{
		"namespace": {revision.Namespace},
		"revision":  {revision.Name},
		"version":   {version},
	}
	router := r.router
	if router == "" {
		router = routerReadyURL
	}
	requestCtx, cancel := context.WithTimeout(ctx, time.Second)
	defer cancel()
	request, err := http.NewRequestWithContext(
		requestCtx,
		http.MethodGet,
		router+"?"+query.Encode(),
		nil,
	)
	if err != nil {
		return err
	}
	client := r.http
	if client == nil {
		client = http.DefaultClient
	}
	response, err := client.Do(request)
	if err != nil {
		return fmt.Errorf("check router Revision projection: %w", err)
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK {
		_, _ = io.Copy(io.Discard, response.Body)
		return fmt.Errorf("router Revision projection is not ready: %s", response.Status)
	}
	return nil
}

func (r *Reconciler) FinalizeKind(ctx context.Context, revision *servingv1.Revision) reconciler.Event {
	configMaps := r.kube.CoreV1().ConfigMaps(systemNamespace)
	err := retry.RetryOnConflict(retry.DefaultRetry, func() error {
		config, err := configMaps.Get(ctx, "lampyr-routes", metav1.GetOptions{})
		if apierrors.IsNotFound(err) {
			return nil
		}
		if err != nil {
			return err
		}
		if _, exists := config.Data[revisionMetadataKey(revision)]; !exists {
			return nil
		}
		delete(config.Data, revisionMetadataKey(revision))
		_, err = configMaps.Update(ctx, config, metav1.UpdateOptions{})
		return err
	})
	if err != nil {
		return fmt.Errorf("remove Revision router metadata: %w", err)
	}
	return nil
}
