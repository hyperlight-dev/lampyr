// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package servingrevision

import (
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/rsa"
	"crypto/sha256"
	"crypto/x509"
	"encoding/base64"
	"encoding/json"
	"encoding/pem"
	"errors"
	"fmt"
	"io"
	"math/big"
	"net/http"
	"net/http/httptest"
	"os"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/Azure/fiberd/pkg/tlsconf"
	"github.com/google/go-containerregistry/pkg/name"
	"github.com/hyperlight-dev/lampyr/internal/project"
	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/api/resource"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/util/intstr"
	utilyaml "k8s.io/apimachinery/pkg/util/yaml"
	dynamicfake "k8s.io/client-go/dynamic/fake"
	"k8s.io/client-go/kubernetes/fake"
	clienttesting "k8s.io/client-go/testing"
	"knative.dev/networking/pkg/apis/networking"
	"knative.dev/pkg/controller"
	"knative.dev/pkg/logging"
	"knative.dev/serving/pkg/apis/autoscaling"
	servingv1 "knative.dev/serving/pkg/apis/serving/v1"
)

func TestInvalidRevisionIsRejected(t *testing.T) {
	revision := &servingv1.Revision{}
	if err := (&Reconciler{}).ReconcileKind(context.Background(), revision); err != nil {
		t.Fatal(err)
	}
	if revision.IsReady() {
		t.Fatal("invalid Revision reconciliation reported Ready")
	}
	condition := revision.Status.GetCondition(servingv1.RevisionConditionReady)
	if condition == nil || condition.IsUnknown() || condition.IsTrue() {
		t.Fatalf("Ready condition = %#v, want False", condition)
	}
}

func TestValidateRevisionUsesPortableImageContract(t *testing.T) {
	concurrency := int64(1)
	revision := &servingv1.Revision{
		ObjectMeta: metav1.ObjectMeta{
			Namespace: "default",
			Annotations: map[string]string{
				templateAnnotation:                "registry.example/app@sha256:" + digest,
				autoscaling.MaxScaleAnnotationKey: "7",
			},
		},
		Spec: servingv1.RevisionSpec{
			ContainerConcurrency: &concurrency,
			PodSpec: corev1.PodSpec{Containers: []corev1.Container{{
				Name:  "user-container",
				Image: "registry.example/app@sha256:" + digest,
			}}},
		},
	}
	spec, err := validateRevision(revision)
	if err != nil {
		t.Fatal(err)
	}
	if spec.containerImage != revision.Spec.Containers[0].Image ||
		spec.template.String() != revision.Annotations[templateAnnotation] ||
		spec.maxFibers != 7 ||
		spec.execution.Mode != project.ExecutionFresh ||
		spec.timeout != 5*time.Minute {
		t.Fatalf("validated spec = %#v", spec)
	}
}

func TestValidateRevisionRejectsUnenforcedSemantics(t *testing.T) {
	concurrency := int64(1)
	base := &servingv1.Revision{
		ObjectMeta: metav1.ObjectMeta{
			Namespace: "default",
			Annotations: map[string]string{
				templateAnnotation: "registry.example/app@sha256:" + digest,
			},
		},
		Spec: servingv1.RevisionSpec{
			ContainerConcurrency: &concurrency,
			PodSpec: corev1.PodSpec{Containers: []corev1.Container{{
				Name:  "user-container",
				Image: "registry.example/app@sha256:" + digest,
			}}},
		},
	}
	for _, test := range []struct {
		name   string
		mutate func(*servingv1.Revision)
	}{
		{"command", func(revision *servingv1.Revision) {
			revision.Spec.Containers[0].Command = []string{"override"}
		}},
		{"environment", func(revision *servingv1.Revision) {
			revision.Spec.Containers[0].Env = []corev1.EnvVar{{Name: "OVERRIDE", Value: "true"}}
		}},
		{"readiness probe", func(revision *servingv1.Revision) {
			revision.Spec.Containers[0].ReadinessProbe = &corev1.Probe{
				ProbeHandler: corev1.ProbeHandler{
					HTTPGet: &corev1.HTTPGetAction{Path: "/ready"},
				},
			}
		}},
		{"liveness probe", func(revision *servingv1.Revision) {
			revision.Spec.Containers[0].LivenessProbe = &corev1.Probe{
				ProbeHandler: corev1.ProbeHandler{
					TCPSocket: &corev1.TCPSocketAction{Port: intstr.FromInt(8080)},
				},
			}
		}},
		{"service account", func(revision *servingv1.Revision) {
			revision.Spec.ServiceAccountName = "app-reader"
		}},
		{"container security context", func(revision *servingv1.Revision) {
			user := int64(1000)
			revision.Spec.Containers[0].SecurityContext = &corev1.SecurityContext{
				RunAsUser: &user,
			}
		}},
		{"response start timeout", func(revision *servingv1.Revision) {
			timeout := int64(5)
			revision.Spec.ResponseStartTimeoutSeconds = &timeout
		}},
		{"idle timeout", func(revision *servingv1.Revision) {
			timeout := int64(5)
			revision.Spec.IdleTimeoutSeconds = &timeout
		}},
		{"CPU limit", func(revision *servingv1.Revision) {
			revision.Spec.Containers[0].Resources.Limits = corev1.ResourceList{
				corev1.ResourceCPU: resource.MustParse("100m"),
			}
		}},
		{"memory limit", func(revision *servingv1.Revision) {
			revision.Spec.Containers[0].Resources.Limits = corev1.ResourceList{
				corev1.ResourceMemory: resource.MustParse("256Mi"),
			}
		}},
		{"concurrency", func(revision *servingv1.Revision) {
			zero := int64(0)
			revision.Spec.ContainerConcurrency = &zero
		}},
		{"minScale", func(revision *servingv1.Revision) {
			revision.Annotations[autoscaling.MinScaleAnnotationKey] = "1"
		}},
		{"autoscaling metric", func(revision *servingv1.Revision) {
			revision.Annotations[autoscaling.MetricAnnotationKey] = "rps"
		}},
		{"unlimited maxScale", func(revision *servingv1.Revision) {
			revision.Annotations[autoscaling.MaxScaleAnnotationKey] = "0"
		}},
		{"runtime class", func(revision *servingv1.Revision) {
			runtimeClass := "gvisor"
			revision.Spec.RuntimeClassName = &runtimeClass
		}},
	} {
		t.Run(test.name, func(t *testing.T) {
			revision := base.DeepCopy()
			test.mutate(revision)
			if _, err := validateRevision(revision); err == nil {
				t.Fatalf("Revision with unsupported %s was accepted", test.name)
			}
		})
	}
}

func TestValidateRevisionSerializesMemoryAsBytes(t *testing.T) {
	concurrency := int64(1)
	revision := &servingv1.Revision{
		ObjectMeta: metav1.ObjectMeta{
			Namespace: "default",
			Annotations: map[string]string{
				templateAnnotation: "registry.example/app@sha256:" + digest,
			},
		},
		Spec: servingv1.RevisionSpec{
			ContainerConcurrency: &concurrency,
			PodSpec: corev1.PodSpec{Containers: []corev1.Container{{
				Name:  "user-container",
				Image: "registry.example/app@sha256:" + digest,
				Resources: corev1.ResourceRequirements{Requests: corev1.ResourceList{
					corev1.ResourceMemory: resource.MustParse("1e9"),
				}},
			}}},
		},
	}
	spec, err := validateRevision(revision)
	if err != nil {
		t.Fatal(err)
	}
	if spec.memory != "1000000000" {
		t.Fatalf("memory = %q, want integer bytes", spec.memory)
	}
}

func TestExistingChildrenMustBelongToCurrentRevision(t *testing.T) {
	revision := &servingv1.Revision{
		ObjectMeta: metav1.ObjectMeta{
			Name:      "demo-00001",
			Namespace: "default",
			UID:       "current-revision",
		},
	}
	service := &corev1.Service{
		ObjectMeta: metav1.ObjectMeta{Name: revision.Name, Namespace: revision.Namespace},
		Spec: corev1.ServiceSpec{
			Type:         corev1.ServiceTypeExternalName,
			ExternalName: routerService,
			Ports:        []corev1.ServicePort{{Name: "http", Port: 80}},
		},
	}
	reconciler := &Reconciler{kube: fake.NewSimpleClientset(service)}
	if err := reconciler.reconcileBackendService(
		context.Background(),
		revision,
		networking.ProtocolHTTP1,
	); err == nil {
		t.Fatal("ownerless backend Service was accepted")
	}
	service.OwnerReferences = []metav1.OwnerReference{*metav1.NewControllerRef(
		revision,
		servingv1.SchemeGroupVersion.WithKind("Revision"),
	)}
	service.Spec.Ports[0] = corev1.ServicePort{
		Name:        "http2",
		AppProtocol: &networking.AppProtocolH2C,
		Port:        80,
		TargetPort:  intstr.FromInt(80),
		Protocol:    corev1.ProtocolTCP,
	}
	reconciler = &Reconciler{kube: fake.NewSimpleClientset(service)}
	if err := reconciler.reconcileBackendService(
		context.Background(),
		revision,
		networking.ProtocolH2C,
	); err != nil {
		t.Fatal(err)
	}
	updated, err := reconciler.kube.CoreV1().Services("default").Get(
		context.Background(),
		revision.Name,
		metav1.GetOptions{},
	)
	if err != nil {
		t.Fatal(err)
	}
	if updated.Spec.ExternalName != routerH2CService ||
		updated.Spec.Ports[0].Port != int32(networking.ServicePort(networking.ProtocolH2C)) {
		t.Fatalf("reconciled backend Service = %#v", updated.Spec)
	}

	desired, err := desiredCapacityGrant(revision, testSpec(t), testHome())
	if err != nil {
		t.Fatal(err)
	}
	current := desired.DeepCopy()
	if err := ensureGrantIdentity(current, desired); err != nil {
		t.Fatal(err)
	}
	other := current.DeepCopy()
	if err := unstructured.SetNestedField(other.Object, "sha256:"+fmt.Sprintf("%064d", 0), "spec", "zygote", "digest"); err != nil {
		t.Fatal(err)
	}
	if err := ensureGrantIdentity(other, desired); err == nil {
		t.Fatal("CapacityGrant serving another template was accepted")
	}
	current.SetOwnerReferences(nil)
	if err := ensureGrantIdentity(current, desired); err == nil {
		t.Fatal("ownerless CapacityGrant was accepted")
	}
}

func TestValidateRevisionUsesKnativeScaleAliases(t *testing.T) {
	concurrency := int64(1)
	base := &servingv1.Revision{
		ObjectMeta: metav1.ObjectMeta{
			Namespace: "default",
			Annotations: map[string]string{
				templateAnnotation: "registry.example/app@sha256:" + digest,
			},
		},
		Spec: servingv1.RevisionSpec{
			ContainerConcurrency: &concurrency,
			PodSpec: corev1.PodSpec{Containers: []corev1.Container{{
				Name:  "user-container",
				Image: "registry.example/app@sha256:" + digest,
			}}},
		},
	}
	max := base.DeepCopy()
	max.Annotations["autoscaling.knative.dev/maxScale"] = "2"
	spec, err := validateRevision(max)
	if err != nil {
		t.Fatal(err)
	}
	if spec.maxFibers != 2 {
		t.Fatalf("maxFibers = %d, want 2", spec.maxFibers)
	}
	min := base.DeepCopy()
	min.Annotations["autoscaling.knative.dev/minScale"] = "1"
	if _, err := validateRevision(min); err == nil {
		t.Fatal("minScale alias bypassed reserved-capacity rejection")
	}
}

func TestCapacityGrantReadinessNeedsCurrentRolledOutSpec(t *testing.T) {
	grant := func(generation, observed int64, ready, progressing, reason string) *unstructured.Unstructured {
		// ready is a status, or status:reason when the reason matters.
		ready, readyReason, found := strings.Cut(ready, ":")
		if !found {
			readyReason = "Available"
		}
		object := &unstructured.Unstructured{Object: map[string]any{
			"status": map[string]any{
				"observedGeneration": observed,
				"conditions": []any{
					map[string]any{"type": "Ready", "status": ready, "reason": readyReason},
					map[string]any{"type": "Progressing", "status": progressing, "reason": reason},
				},
			},
		}}
		object.SetGeneration(generation)
		return object
	}
	// The Deployment of a finished rollout of its current template.
	rolledOut := func() *appsv1.Deployment {
		replicas := int32(1)
		return &appsv1.Deployment{
			ObjectMeta: metav1.ObjectMeta{Name: "fiberd-demo-00001", Generation: 4},
			Spec:       appsv1.DeploymentSpec{Replicas: &replicas},
			Status: appsv1.DeploymentStatus{
				ObservedGeneration: 4, Replicas: 1, UpdatedReplicas: 1, AvailableReplicas: 1,
			},
		}
	}
	for _, test := range []struct {
		name          string
		grant         *unstructured.Unstructured
		ready, failed bool
	}{
		{"rolled out", grant(2, 2, "True", "True", "NewReplicaSetAvailable"), true, false},
		{"earlier generation", grant(3, 2, "True", "True", "NewReplicaSetAvailable"), false, false},
		{"old Pod still serving", grant(2, 2, "True", "True", "ReplicaSetUpdated"), false, false},
		{"not ready", grant(2, 2, "False", "True", "NewReplicaSetAvailable"), false, false},
		{"rollout gave up", grant(2, 2, "True", "False", "ProgressDeadlineExceeded"), false, true},
		{"unavailable", grant(2, 2, "False:Unavailable", "True", "ReplicaSetUpdated"), false, false},
		{"invalid spec", grant(2, 2, "False:InvalidSpec", "Unknown", "InvalidSpec"), false, true},
		{"forbidden", grant(2, 2, "False:Forbidden", "Unknown", "Forbidden"), false, true},
		{"Deployment conflict", grant(2, 2, "False:DeploymentConflict", "Unknown", "DeploymentConflict"), false, true},
		{"Service conflict", grant(2, 2, "False:ServiceConflict", "Unknown", "ServiceConflict"), false, true},
	} {
		ready, failed, message := capacityGrantReadiness(test.grant, rolledOut())
		if ready != test.ready || failed != test.failed || (!ready && message == "") {
			t.Errorf("%s: ready=%v failed=%v message=%q", test.name, ready, failed, message)
		}
	}

	// The pass that first observes a new spec reports the old Pod's finished
	// rollout under the new generation, but has already applied the new
	// template, so the Deployment itself still rolls out.
	stale := grant(2, 2, "True", "True", "NewReplicaSetAvailable")
	unobserved := rolledOut()
	unobserved.Generation = 5
	recreating := rolledOut()
	recreating.Generation, recreating.Status.ObservedGeneration = 5, 5
	recreating.Status.UpdatedReplicas, recreating.Status.AvailableReplicas = 0, 0
	surging := rolledOut()
	surging.Generation, surging.Status.ObservedGeneration = 5, 5
	surging.Status.Replicas = 2
	for _, test := range []struct {
		name       string
		deployment *appsv1.Deployment
	}{
		{"Deployment not created", nil},
		{"template not observed", unobserved},
		{"old Pod stopped, new one not available", recreating},
		{"old Pod still running beside the new one", surging},
	} {
		ready, failed, message := capacityGrantReadiness(stale, test.deployment)
		if ready || failed || !strings.Contains(message, "Deployment") {
			t.Errorf("%s: ready=%v failed=%v message=%q", test.name, ready, failed, message)
		}
	}
}

func TestCapacityGrantAPIFailureReturnsForWorkQueueRetry(t *testing.T) {
	concurrency := int64(1)
	revision := &servingv1.Revision{
		ObjectMeta: metav1.ObjectMeta{
			Name:      "demo-00001",
			Namespace: "default",
			UID:       "revision-uid",
			Labels: map[string]string{
				"serving.knative.dev/service":       "demo",
				"serving.knative.dev/configuration": "demo",
			},
			Annotations: map[string]string{
				templateAnnotation:                "registry.example/app@sha256:" + digest,
				autoscaling.MaxScaleAnnotationKey: "2",
			},
		},
		Spec: servingv1.RevisionSpec{
			ContainerConcurrency: &concurrency,
			PodSpec: corev1.PodSpec{Containers: []corev1.Container{{
				Name:  "user-container",
				Image: "registry.example/app@sha256:" + digest,
			}}},
		},
	}
	spec, err := validateRevision(revision)
	if err != nil {
		t.Fatal(err)
	}
	current, err := desiredCapacityGrant(revision, spec, testHome())
	if err != nil {
		t.Fatal(err)
	}
	if err := unstructured.SetNestedField(current.Object, int64(1), "spec", "fibers", "max"); err != nil {
		t.Fatal(err)
	}
	current.SetAnnotations(map[string]string{grantSpecAnnotation: "stale"})
	client := dynamicfake.NewSimpleDynamicClient(runtime.NewScheme(), current)
	client.PrependReactor("update", "capacitygrants", func(
		clienttesting.Action,
	) (bool, runtime.Object, error) {
		return true, nil, errors.New("temporary API failure")
	})
	reconciler := &Reconciler{
		kube: fake.NewSimpleClientset(&corev1.ConfigMap{
			ObjectMeta: metav1.ObjectMeta{Name: "lampyr-install", Namespace: "kube-system"},
			Data:       map[string]string{"grantImage": "fiberd", "helperImage": "helper"},
		}, routerCertificate(t, newKey(t))),
		dynamic: client,
	}
	if err := reconciler.ReconcileKind(context.Background(), revision); err == nil {
		t.Fatal("CapacityGrant API failure was acknowledged without retry")
	}
}

type nopReconciler struct{}

func (nopReconciler) Reconcile(context.Context, string) error { return nil }

// A raised max-scale reaches the existing grant as one Update carrying the
// resized grant Pod and the new spec digest, which is what tells
// fiberd-k8s-controller to roll it.
func TestReconcileResizesTheGrantWhenMaxScaleChanges(t *testing.T) {
	concurrency := int64(1)
	revision := &servingv1.Revision{
		ObjectMeta: metav1.ObjectMeta{
			Name:      "demo-00001",
			Namespace: "default",
			UID:       "revision-uid",
			Labels: map[string]string{
				"serving.knative.dev/service":       "demo",
				"serving.knative.dev/configuration": "demo",
			},
			Annotations: map[string]string{
				templateAnnotation:                "registry.example/app@sha256:" + digest,
				autoscaling.MaxScaleAnnotationKey: "3",
			},
		},
		Spec: servingv1.RevisionSpec{
			ContainerConcurrency: &concurrency,
			PodSpec: corev1.PodSpec{Containers: []corev1.Container{{
				Name:  "user-container",
				Image: "registry.example/app@sha256:" + digest,
			}}},
		},
	}
	ctx := context.Background()
	kube := fake.NewSimpleClientset(&corev1.ConfigMap{
		ObjectMeta: metav1.ObjectMeta{Name: "lampyr-install", Namespace: "kube-system"},
		Data:       map[string]string{"grantImage": "fiberd", "helperImage": "helper"},
	}, routerCertificate(t, newKey(t)))
	impl := controller.NewContext(ctx, nopReconciler{}, controller.ControllerOptions{Logger: logging.FromContext(ctx)})
	t.Cleanup(impl.WorkQueue().ShutDown)
	reconciler := &Reconciler{kube: kube, impl: impl}
	home, err := reconciler.homeConfiguration(ctx)
	if err != nil {
		t.Fatal(err)
	}
	spec, err := validateRevision(revision)
	if err != nil {
		t.Fatal(err)
	}
	current, err := desiredCapacityGrant(revision, spec, home)
	if err != nil {
		t.Fatal(err)
	}
	client := dynamicfake.NewSimpleDynamicClient(runtime.NewScheme(), current)
	reconciler.dynamic = client

	revision.Annotations[autoscaling.MaxScaleAnnotationKey] = "1000"
	if err := reconciler.ReconcileKind(ctx, revision); err != nil {
		t.Fatal(err)
	}
	var updates []*unstructured.Unstructured
	for _, action := range client.Actions() {
		if update, ok := action.(clienttesting.UpdateAction); ok {
			updates = append(updates, update.GetObject().(*unstructured.Unstructured))
		}
	}
	if len(updates) != 1 {
		t.Fatalf("CapacityGrant updates = %d, want 1", len(updates))
	}
	updated := updates[0]
	if fibers, _, _ := unstructured.NestedInt64(updated.Object, "spec", "fibers", "max"); fibers != 1000 {
		t.Errorf("fibers.max = %d, want 1000", fibers)
	}
	containers, _, _ := unstructured.NestedSlice(updated.Object, "spec", "podTemplate", "spec", "containers")
	container := containers[0].(map[string]any)
	ceiling := int64(16000) << 20
	if args := container["args"].([]any); len(args) != 1 || args[0] != "-grant-ceiling="+strconv.FormatInt(ceiling, 10) {
		t.Errorf("args = %v, want the 1000-Fiber ceiling", args)
	}
	resources := container["resources"].(map[string]any)
	limits := resources["limits"].(map[string]any)
	requests := resources["requests"].(map[string]any)
	if limits["memory"] != strconv.FormatInt(ceiling+homeMemoryHeadroom, 10) || requests["cpu"] != "1000m" {
		t.Errorf("resources = %v, want the 1000-Fiber memory and cpu", resources)
	}
	spec.maxFibers = 1000
	want, err := desiredCapacityGrant(revision, spec, home)
	if err != nil {
		t.Fatal(err)
	}
	specDigest := updated.GetAnnotations()[grantSpecAnnotation]
	if specDigest == current.GetAnnotations()[grantSpecAnnotation] || specDigest != want.GetAnnotations()[grantSpecAnnotation] {
		t.Errorf("grant-spec annotation = %q, want %q", specDigest, want.GetAnnotations()[grantSpecAnnotation])
	}
}

// A grant whose status reports the new generation Ready from the old Pod
// stays pending while its Deployment rolls out the new template.
func TestReconcileWaitsForTheGrantDeploymentRollout(t *testing.T) {
	concurrency := int64(1)
	revision := &servingv1.Revision{
		ObjectMeta: metav1.ObjectMeta{
			Name:      "demo-00001",
			Namespace: "default",
			UID:       "revision-uid",
			Labels: map[string]string{
				"serving.knative.dev/service":       "demo",
				"serving.knative.dev/configuration": "demo",
			},
			Annotations: map[string]string{
				templateAnnotation:                "registry.example/app@sha256:" + digest,
				autoscaling.MaxScaleAnnotationKey: "3",
			},
		},
		Spec: servingv1.RevisionSpec{
			ContainerConcurrency: &concurrency,
			PodSpec: corev1.PodSpec{Containers: []corev1.Container{{
				Name:  "user-container",
				Image: "registry.example/app@sha256:" + digest,
			}}},
		},
	}
	ctx := context.Background()
	replicas := int32(1)
	deployment := &appsv1.Deployment{
		ObjectMeta: metav1.ObjectMeta{Name: "fiberd-demo-00001", Namespace: "default", Generation: 5},
		Spec:       appsv1.DeploymentSpec{Replicas: &replicas},
		Status: appsv1.DeploymentStatus{
			ObservedGeneration: 4, Replicas: 1, UpdatedReplicas: 1, AvailableReplicas: 1,
		},
	}
	kube := fake.NewSimpleClientset(&corev1.ConfigMap{
		ObjectMeta: metav1.ObjectMeta{Name: "lampyr-install", Namespace: "kube-system"},
		Data:       map[string]string{"grantImage": "fiberd", "helperImage": "helper"},
	}, routerCertificate(t, newKey(t)), deployment)
	impl := controller.NewContext(ctx, nopReconciler{}, controller.ControllerOptions{Logger: logging.FromContext(ctx)})
	t.Cleanup(impl.WorkQueue().ShutDown)
	reconciler := &Reconciler{kube: kube, impl: impl}
	home, err := reconciler.homeConfiguration(ctx)
	if err != nil {
		t.Fatal(err)
	}
	spec, err := validateRevision(revision)
	if err != nil {
		t.Fatal(err)
	}
	current, err := desiredCapacityGrant(revision, spec, home)
	if err != nil {
		t.Fatal(err)
	}
	current.SetGeneration(2)
	current.Object["status"] = map[string]any{
		"observedGeneration": int64(2),
		"conditions": []any{
			map[string]any{"type": "Ready", "status": "True", "reason": "Available"},
			map[string]any{"type": "Progressing", "status": "True", "reason": "NewReplicaSetAvailable"},
		},
	}
	reconciler.dynamic = dynamicfake.NewSimpleDynamicClient(runtime.NewScheme(), current)

	if err := reconciler.ReconcileKind(ctx, revision); err != nil {
		t.Fatal(err)
	}
	condition := revision.Status.GetCondition(servingv1.RevisionConditionResourcesAvailable)
	if condition == nil || condition.Reason != "CapacityGrantPending" ||
		!strings.Contains(condition.Message, "Deployment is rolling out") {
		t.Fatalf("ResourcesAvailable = %+v, want pending on the Deployment rollout", condition)
	}

	deployment.Status.ObservedGeneration = 5
	if _, err := kube.AppsV1().Deployments("default").UpdateStatus(ctx, deployment, metav1.UpdateOptions{}); err != nil {
		t.Fatal(err)
	}
	if err := reconciler.ReconcileKind(ctx, revision); err != nil {
		t.Fatal(err)
	}
	// Past the grant, the Revision waits on the router, which this test
	// doesn't run.
	condition = revision.Status.GetCondition(servingv1.RevisionConditionResourcesAvailable)
	if condition == nil || condition.Reason != "RouterMetadataPending" {
		t.Fatalf("ResourcesAvailable = %+v, want the grant ready", condition)
	}
}

func TestValidateRevisionDetectsH2CProtocol(t *testing.T) {
	concurrency := int64(1)
	revision := &servingv1.Revision{
		ObjectMeta: metav1.ObjectMeta{
			Namespace: "default",
			Annotations: map[string]string{
				templateAnnotation: "registry.example/app@sha256:" + digest,
			},
		},
		Spec: servingv1.RevisionSpec{
			ContainerConcurrency: &concurrency,
			PodSpec: corev1.PodSpec{Containers: []corev1.Container{{
				Name:  "user-container",
				Image: "registry.example/app@sha256:" + digest,
				Ports: []corev1.ContainerPort{{Name: "h2c", ContainerPort: 8080}},
			}}},
		},
	}
	spec, err := validateRevision(revision)
	if err != nil {
		t.Fatal(err)
	}
	if spec.protocol != networking.ProtocolH2C {
		t.Fatalf("protocol = %q, want h2c", spec.protocol)
	}
}

func TestH2CBackendUsesKnativeServicePort(t *testing.T) {
	revision := &servingv1.Revision{
		ObjectMeta: metav1.ObjectMeta{
			Name:      "grpc-00001",
			Namespace: "default",
			UID:       "revision-uid",
		},
	}
	kube := fake.NewSimpleClientset()
	reconciler := &Reconciler{kube: kube}
	if err := reconciler.reconcileBackendService(
		context.Background(),
		revision,
		networking.ProtocolH2C,
	); err != nil {
		t.Fatal(err)
	}
	service, err := kube.CoreV1().Services("default").Get(
		context.Background(),
		revision.Name,
		metav1.GetOptions{},
	)
	if err != nil {
		t.Fatal(err)
	}
	if len(service.Spec.Ports) != 1 ||
		service.Spec.Ports[0].Port != int32(networking.ServicePort(networking.ProtocolH2C)) ||
		service.Spec.ExternalName != routerH2CService {
		t.Fatalf("h2c backend ports = %#v", service.Spec.Ports)
	}
}

func TestFiberRouterServiceExposesKnativeH2CPort(t *testing.T) {
	manifest, err := os.Open("../../k8s/fiber-router.yaml")
	if err != nil {
		t.Fatal(err)
	}
	defer manifest.Close()
	decoder := utilyaml.NewYAMLOrJSONDecoder(manifest, 4096)
	for {
		var resource unstructured.Unstructured
		if err := decoder.Decode(&resource); err != nil {
			if errors.Is(err, io.EOF) {
				break
			}
			t.Fatal(err)
		}
		if resource.GetKind() != "Service" || resource.GetName() != "fiber-router-h2c" {
			continue
		}
		ports, _, _ := unstructured.NestedSlice(resource.Object, "spec", "ports")
		for _, raw := range ports {
			port := raw.(map[string]any)
			if port["name"] == "h2c" && port["port"] == int64(networking.ServicePort(networking.ProtocolH2C)) {
				return
			}
		}
		t.Fatalf("fiber-router-h2c ports = %#v", ports)
	}
	t.Fatal("fiber-router-h2c Service not found")
}

func TestHomePodMetadataIsValidatedAndMerged(t *testing.T) {
	metadata, err := parseHomePodMetadata(
		`{"labels":{"azure.workload.identity/use":"true"},"annotations":{"example.dev/profile":"private"}}`,
	)
	if err != nil {
		t.Fatal(err)
	}
	revision := &servingv1.Revision{
		ObjectMeta: metav1.ObjectMeta{
			Name:      "demo-00001",
			Namespace: "default",
			Labels: map[string]string{
				"serving.knative.dev/service":       "demo",
				"serving.knative.dev/configuration": "demo",
			},
		},
	}
	home := testHome()
	home.metadata = metadata
	grant, err := desiredCapacityGrant(revision, testSpec(t), home)
	if err != nil {
		t.Fatal(err)
	}
	labels, _, err := unstructured.NestedStringMap(grant.Object, "spec", "podTemplate", "metadata", "labels")
	if err != nil {
		t.Fatal(err)
	}
	annotations, _, err := unstructured.NestedStringMap(
		grant.Object,
		"spec",
		"podTemplate",
		"metadata",
		"annotations",
	)
	if err != nil {
		t.Fatal(err)
	}
	if labels["azure.workload.identity/use"] != "true" ||
		annotations["example.dev/profile"] != "private" ||
		labels["serving.knative.dev/revision"] != revision.Name {
		t.Fatalf("Home metadata labels=%#v annotations=%#v", labels, annotations)
	}
	if _, err := parseHomePodMetadata(`{"labels":{"lampyr.dev/home":"false"}}`); err == nil {
		t.Fatal("reserved Home label override was accepted")
	}
	// fiberd-k8s-controller refuses these keys, so it would refuse every
	// grant that carried one.
	for _, document := range []string{
		`{"annotations":{"container.apparmor.security.beta.kubernetes.io/fiberd":"localhost/profile"}}`,
		`{"annotations":{"k8s.v1.cni.cncf.io/networks":"host"}}`,
		`{"labels":{"fiberd.io/grant":"other"}}`,
		`{"labels":{"policy.fiberd.io/tier":"gold"}}`,
		`{"annotations":{"io.kubernetes.cri-o.Devices":"/dev/kvm"}}`,
	} {
		if _, err := parseHomePodMetadata(document); err == nil || !strings.Contains(err.Error(), "reserved by fiberd-k8s-controller") {
			t.Fatalf("homePodMetadata %s = %v, want it refused as reserved", document, err)
		}
	}
}

func TestValidateRevisionRequiresFiberdTemplate(t *testing.T) {
	revision := &servingv1.Revision{
		ObjectMeta: metav1.ObjectMeta{Namespace: "default"},
		Spec: servingv1.RevisionSpec{
			PodSpec: corev1.PodSpec{Containers: []corev1.Container{{
				Image: "registry.example/app@sha256:" + digest,
			}}},
		},
	}
	if _, err := validateRevision(revision); err == nil {
		t.Fatal("Revision without a fiberd template was accepted")
	}
}

func TestRevisionRouterMetadataLifecycle(t *testing.T) {
	ctx := context.Background()
	client := fake.NewSimpleClientset(&corev1.ConfigMap{
		ObjectMeta: metav1.ObjectMeta{Name: "lampyr-routes", Namespace: systemNamespace},
	})
	reconciler := &Reconciler{kube: client}
	revision := &servingv1.Revision{
		ObjectMeta: metav1.ObjectMeta{
			Name:      "demo-00001",
			Namespace: "default",
			Labels: map[string]string{
				"serving.knative.dev/routingState": "active",
			},
		},
	}
	spec := revisionSpec{
		execution: project.ExecutionPolicy{
			Mode:        project.ExecutionReuse,
			IdleTimeout: "30s",
		},
		timeout: 15 * time.Second,
	}
	version, err := reconciler.publishRevision(ctx, revision, spec)
	if err != nil {
		t.Fatal(err)
	}
	config, err := client.CoreV1().ConfigMaps(systemNamespace).Get(
		ctx,
		"lampyr-routes",
		metav1.GetOptions{},
	)
	if err != nil {
		t.Fatal(err)
	}
	var document struct {
		Kind      string                  `json:"kind"`
		Namespace string                  `json:"namespace"`
		Revision  string                  `json:"revision"`
		Execution project.ExecutionPolicy `json:"execution"`
		Timeout   string                  `json:"timeout"`
		Routed    bool                    `json:"routed"`
	}
	key := revisionMetadataKey(revision)
	if err := json.Unmarshal([]byte(config.Data[key]), &document); err != nil {
		t.Fatal(err)
	}
	if document.Kind != "revision" ||
		document.Namespace != revision.Namespace ||
		document.Revision != revision.Name ||
		document.Execution != spec.execution ||
		document.Timeout != spec.timeout.String() ||
		!document.Routed {
		t.Fatalf("router metadata = %#v", document)
	}
	router := httptest.NewServer(http.HandlerFunc(func(
		response http.ResponseWriter,
		request *http.Request,
	) {
		if request.URL.Query().Get("namespace") != revision.Namespace ||
			request.URL.Query().Get("revision") != revision.Name ||
			request.URL.Query().Get("version") != version {
			http.Error(response, "unexpected projection", http.StatusServiceUnavailable)
		}
	}))
	defer router.Close()
	reconciler.http = router.Client()
	reconciler.router = router.URL
	if err := reconciler.waitForRouter(ctx, revision, version); err != nil {
		t.Fatal(err)
	}
	if err := reconciler.FinalizeKind(ctx, revision); err != nil {
		t.Fatal(err)
	}
	config, err = client.CoreV1().ConfigMaps(systemNamespace).Get(
		ctx,
		"lampyr-routes",
		metav1.GetOptions{},
	)
	if err != nil {
		t.Fatal(err)
	}
	if _, exists := config.Data[key]; exists {
		t.Fatal("router metadata survived Revision finalization")
	}
}

const digest = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

func testSpec(t *testing.T) revisionSpec {
	t.Helper()
	template, err := name.NewDigest("localhost:5001/demo@sha256:" + digest)
	if err != nil {
		t.Fatal(err)
	}
	return revisionSpec{template: template, containerImage: "container", maxFibers: 3, memory: "536870912"}
}

func testHome() homeConfig {
	return homeConfig{
		grantImage: "registry.example/fiberd@sha256:" + digest, helperImage: "registry.example/helper@sha256:" + digest,
		callerThumbprint: "NzbLsXh8uDCcd-6MNwXF4W_7noWXFZAfHkxZsRGC9Xs",
	}
}

func newKey(t *testing.T) *ecdsa.PrivateKey {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	return key
}

// certificatePEM self-signs a certificate over key, as cert-manager writes
// one into tls.crt.
func certificatePEM(t *testing.T, key *ecdsa.PrivateKey, serial int64) []byte {
	t.Helper()
	template := &x509.Certificate{SerialNumber: big.NewInt(serial), NotBefore: time.Now(), NotAfter: time.Now().Add(time.Hour)}
	der, err := x509.CreateCertificate(rand.Reader, template, template, &key.PublicKey, key)
	if err != nil {
		t.Fatal(err)
	}
	return pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der})
}

func routerCertificate(t *testing.T, key *ecdsa.PrivateKey) *corev1.Secret {
	return &corev1.Secret{
		ObjectMeta: metav1.ObjectMeta{Name: routerCertSecret, Namespace: systemNamespace},
		Type:       corev1.SecretTypeTLS,
		Data:       map[string][]byte{corev1.TLSCertKey: certificatePEM(t, key, 1)},
	}
}

// The thumbprint is fiberd's tlsconf.Thumbprint, as fiberd refuses a
// grant whose cnf jkt names another key; this pins its RFC 7638 form.
func TestCallerThumbprintIsFiberdsRFC7638Thumbprint(t *testing.T) {
	// fiberd's own vector, RFC 7638 section 3.1's RSA key.
	modulus, err := base64.RawURLEncoding.DecodeString("0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiFV4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6Cf0h4QyQ5v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzs8KJZgnYb9c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08qNLyrdkt-bFTWhAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1jF44-csFCur-kEgU8awapJzKnqDKgw")
	if err != nil {
		t.Fatal(err)
	}
	if got, err := tlsconf.Thumbprint(&x509.Certificate{PublicKey: &rsa.PublicKey{N: new(big.Int).SetBytes(modulus), E: 65537}}); err != nil || got != "NzbLsXh8uDCcd-6MNwXF4W_7noWXFZAfHkxZsRGC9Xs" {
		t.Fatalf("RFC 7638 vector = %q, %v", got, err)
	}

	// A P-256 key through its canonical JWK, read from the PEM after a
	// block that is not a certificate.
	key := newKey(t)
	point, err := key.PublicKey.Bytes()
	if err != nil {
		t.Fatal(err)
	}
	b64 := base64.RawURLEncoding
	sum := sha256.Sum256([]byte(`{"crv":"P-256","kty":"EC","x":"` + b64.EncodeToString(point[1:33]) + `","y":"` + b64.EncodeToString(point[33:]) + `"}`))
	want := b64.EncodeToString(sum[:])
	data := append(pem.EncodeToMemory(&pem.Block{Type: "EC PRIVATE KEY", Bytes: []byte("x")}), certificatePEM(t, key, 1)...)
	if got, err := certificateThumbprint(data); err != nil || got != want {
		t.Fatalf("P-256 thumbprint = %q, %v, want %q", got, err, want)
	}
	// A renewal over the same key keeps it, so it rolls no grant Pod.
	if got, err := certificateThumbprint(certificatePEM(t, key, 2)); err != nil || got != want {
		t.Fatalf("renewed thumbprint = %q, %v, want %q", got, err, want)
	}
	if _, err := certificateThumbprint(nil); err == nil {
		t.Fatal("an empty tls.crt has a thumbprint")
	}
}

func TestHomeConfigurationWaitsForTheRoutersCertificate(t *testing.T) {
	kube := fake.NewSimpleClientset(&corev1.ConfigMap{
		ObjectMeta: metav1.ObjectMeta{Name: "lampyr-install", Namespace: "kube-system"},
		Data:       map[string]string{"grantImage": "fiberd", "helperImage": "helper"},
	})
	reconciler := &Reconciler{kube: kube}
	if _, err := reconciler.homeConfiguration(context.Background()); err == nil || !strings.Contains(err.Error(), "fiber-router's certificate") {
		t.Fatalf("configuration without the router's certificate = %v, want it pending", err)
	}
	key := newKey(t)
	if _, err := kube.CoreV1().Secrets(systemNamespace).Create(context.Background(), routerCertificate(t, key), metav1.CreateOptions{}); err != nil {
		t.Fatal(err)
	}
	home, err := reconciler.homeConfiguration(context.Background())
	want, _ := tlsconf.Thumbprint(&x509.Certificate{PublicKey: &key.PublicKey})
	if err != nil || home.callerThumbprint != want {
		t.Fatalf("callerThumbprint = %q, %v, want %q", home.callerThumbprint, err, want)
	}
}

func TestDesiredCapacityGrantUsesTheControllerSchema(t *testing.T) {
	revision := &servingv1.Revision{ObjectMeta: metav1.ObjectMeta{Name: "demo-00001", Namespace: "default", UID: "revision-uid"}}
	home := testHome()
	home.mirrors = map[string]registryMirror{"localhost:5001": {Registry: "lampyr-registry:5000", PlainHTTP: true}}
	grant, err := desiredCapacityGrant(revision, testSpec(t), home)
	if err != nil {
		t.Fatal(err)
	}
	zygote, _, _ := unstructured.NestedStringMap(grant.Object, "spec", "zygote")
	if zygote["registry"] != "http://lampyr-registry:5000/demo" ||
		zygote["digest"] != "sha256:"+digest ||
		zygote["sealKeySecretName"] != sealKeySecret {
		t.Fatalf("zygote = %#v", zygote)
	}
	for path, want := range map[string]string{
		"runtime":          "hyperlight",
		"helperImage":      home.helperImage,
		"writeBudget":      "536870912",
		"callerThumbprint": home.callerThumbprint,
	} {
		if got, _, _ := unstructured.NestedString(grant.Object, "spec", path); got != want {
			t.Fatalf("spec.%s = %q, want %q", path, got, want)
		}
	}
	containers, _, _ := unstructured.NestedSlice(grant.Object, "spec", "podTemplate", "spec", "containers")
	container := containers[0].(map[string]any)
	if len(containers) != 1 || container["name"] != "fiberd" || container["image"] != home.grantImage || container["command"] != nil {
		t.Fatalf("containers = %#v", containers)
	}
	// Each tenant arg is one -flag=value item, as the controller requires.
	for _, arg := range container["args"].([]any) {
		if !strings.HasPrefix(arg.(string), "-") || !strings.Contains(arg.(string), "=") {
			t.Fatalf("arg %q is not one -flag=value item", arg)
		}
	}

	unmapped, err := desiredCapacityGrant(revision, testSpec(t), testHome())
	if err != nil {
		t.Fatal(err)
	}
	if registry, _, _ := unstructured.NestedString(unmapped.Object, "spec", "zygote", "registry"); registry != "localhost:5001/demo" {
		t.Fatalf("unmapped registry = %q", registry)
	}
	if unmapped.GetAnnotations()[grantSpecAnnotation] == grant.GetAnnotations()[grantSpecAnnotation] {
		t.Fatal("a changed spec kept its spec digest")
	}
}

func TestGrantMemoryCeilingFollowsFiberLimit(t *testing.T) {
	revision := &servingv1.Revision{ObjectMeta: metav1.ObjectMeta{Name: "demo-00001", Namespace: "default", UID: "revision-uid"}}
	base, err := desiredCapacityGrant(revision, testSpec(t), testHome())
	if err != nil {
		t.Fatal(err)
	}
	for _, test := range []struct {
		fibers  int64
		ceiling int64
		memory  string
		cpu     string
	}{
		// A grant of up to the default max-scale keeps the default
		// ceiling and requests 10m of CPU, then a millicore per Fiber,
		// at most two CPUs, as the old home did.
		{fibers: 3, ceiling: defaultGrantCeiling, memory: "2Gi", cpu: "10m"},
		{fibers: 100, ceiling: defaultGrantCeiling, memory: "2Gi", cpu: "10m"},
		{fibers: 101, ceiling: 1616 << 20, memory: strconv.FormatInt((1616<<20)+homeMemoryHeadroom, 10), cpu: "101m"},
		{fibers: 1000, ceiling: 16000 << 20, memory: strconv.FormatInt((16000<<20)+homeMemoryHeadroom, 10), cpu: "1000m"},
		{fibers: 4096, ceiling: 65536 << 20, memory: strconv.FormatInt((65536<<20)+homeMemoryHeadroom, 10), cpu: "2000m"},
		{fibers: 100000, ceiling: maxGrantCeiling, memory: strconv.FormatInt(maxGrantCeiling+homeMemoryHeadroom, 10), cpu: "2000m"},
	} {
		spec := testSpec(t)
		spec.maxFibers = test.fibers
		grant, err := desiredCapacityGrant(revision, spec, testHome())
		if err != nil {
			t.Fatal(err)
		}
		containers, _, _ := unstructured.NestedSlice(grant.Object, "spec", "podTemplate", "spec", "containers")
		container := containers[0].(map[string]any)
		args := container["args"].([]any)
		resources := container["resources"].(map[string]any)
		limits := resources["limits"].(map[string]any)
		requests := resources["requests"].(map[string]any)
		// CPU is a request alone, never a limit the Pod is throttled at.
		if _, capped := limits["cpu"]; capped {
			t.Errorf("fibers %d: limits %v set a CPU limit", test.fibers, limits)
		}
		if len(args) != 1 || args[0] != "-grant-ceiling="+strconv.FormatInt(test.ceiling, 10) ||
			limits["memory"] != test.memory || requests["cpu"] != test.cpu || len(requests) != 1 {
			t.Errorf("fibers %d: args %v, resources %v; want ceiling %d, memory %s, cpu request %q",
				test.fibers, args, resources, test.ceiling, test.memory, test.cpu)
		}
		baseContainers, _, _ := unstructured.NestedSlice(base.Object, "spec", "podTemplate", "spec", "containers")
		unchanged := fmt.Sprint(baseContainers) == fmt.Sprint(containers)
		if unchanged != (test.ceiling == defaultGrantCeiling) {
			t.Errorf("fibers %d: grant Pod container unchanged = %v", test.fibers, unchanged)
		}
	}
}

func TestValidateRevisionRefusesNamesTheControllerCannotServe(t *testing.T) {
	concurrency := int64(1)
	revision := &servingv1.Revision{
		ObjectMeta: metav1.ObjectMeta{
			Name:        strings.Repeat("a", maxGrantName+1),
			Namespace:   "default",
			Annotations: map[string]string{templateAnnotation: "registry.example/app@sha256:" + digest},
		},
		Spec: servingv1.RevisionSpec{
			ContainerConcurrency: &concurrency,
			PodSpec: corev1.PodSpec{Containers: []corev1.Container{{
				Name:  "user-container",
				Image: "registry.example/app@sha256:" + digest,
			}}},
		},
	}
	if _, err := validateRevision(revision); err == nil {
		t.Fatal("Revision name longer than a CapacityGrant name was accepted")
	}
	revision.Name = revision.Name[1:]
	if _, err := validateRevision(revision); err != nil {
		t.Fatal(err)
	}
}

// A Reconciler built without a controller, as these tests build it, takes
// every path that requeues the Revision without panicking: each would
// otherwise call EnqueueAfter on a nil controller.
func TestEveryRequeuePathToleratesANilController(t *testing.T) {
	concurrency := int64(1)
	newRevision := func() *servingv1.Revision {
		return &servingv1.Revision{
			ObjectMeta: metav1.ObjectMeta{
				Name:      "demo-00001",
				Namespace: "default",
				UID:       "revision-uid",
				Labels: map[string]string{
					"serving.knative.dev/service":       "demo",
					"serving.knative.dev/configuration": "demo",
				},
				Annotations: map[string]string{
					templateAnnotation:                "registry.example/app@sha256:" + digest,
					autoscaling.MaxScaleAnnotationKey: "3",
				},
			},
			Spec: servingv1.RevisionSpec{
				ContainerConcurrency: &concurrency,
				PodSpec: corev1.PodSpec{Containers: []corev1.Container{{
					Name:  "user-container",
					Image: "registry.example/app@sha256:" + digest,
				}}},
			},
		}
	}
	ctx := context.Background()
	install := &corev1.ConfigMap{
		ObjectMeta: metav1.ObjectMeta{Name: "lampyr-install", Namespace: "kube-system"},
		Data:       map[string]string{"grantImage": "fiberd", "helperImage": "helper"},
	}
	certificate := routerCertificate(t, newKey(t))
	replicas := int32(1)
	rolledOut := &appsv1.Deployment{
		ObjectMeta: metav1.ObjectMeta{Name: "fiberd-demo-00001", Namespace: "default", Generation: 1},
		Spec:       appsv1.DeploymentSpec{Replicas: &replicas},
		Status: appsv1.DeploymentStatus{
			ObservedGeneration: 1, Replicas: 1, UpdatedReplicas: 1, AvailableReplicas: 1,
		},
	}
	// The grant as the Reconciler would write it, with `status`, or with a
	// stale spec for a nil one.
	grant := func(status map[string]any) *unstructured.Unstructured {
		spec, err := validateRevision(newRevision())
		if err != nil {
			t.Fatal(err)
		}
		reconciler := &Reconciler{kube: fake.NewSimpleClientset(install, certificate)}
		home, err := reconciler.homeConfiguration(ctx)
		if err != nil {
			t.Fatal(err)
		}
		current, err := desiredCapacityGrant(newRevision(), spec, home)
		if err != nil {
			t.Fatal(err)
		}
		current.SetGeneration(1)
		if status == nil {
			current.SetAnnotations(map[string]string{grantSpecAnnotation: "stale"})
		} else {
			current.Object["status"] = status
		}
		return current
	}
	conditions := func(ready, reason string) map[string]any {
		return map[string]any{
			"observedGeneration": int64(1),
			"conditions": []any{
				map[string]any{"type": "Ready", "status": ready, "reason": reason},
				map[string]any{"type": "Progressing", "status": "True", "reason": "NewReplicaSetAvailable"},
			},
		}
	}
	for _, test := range []struct {
		name   string
		kube   []runtime.Object
		grants []runtime.Object
		reason string
	}{
		{name: "installation not ready", reason: "InstallationNotReady"},
		{name: "grant created", kube: []runtime.Object{install, certificate}, reason: "CapacityGrantPending"},
		{
			name: "grant updated", kube: []runtime.Object{install, certificate},
			grants: []runtime.Object{grant(nil)}, reason: "CapacityGrantUpdating",
		},
		{
			name: "grant failed", kube: []runtime.Object{install, certificate},
			grants: []runtime.Object{grant(conditions("False", "InvalidSpec"))}, reason: "CapacityGrantFailed",
		},
		{
			name: "grant pending", kube: []runtime.Object{install, certificate},
			grants: []runtime.Object{grant(conditions("False", "Admitting"))}, reason: "CapacityGrantPending",
		},
		// Past the grant, the Revision waits on the router, which this test
		// doesn't run.
		{
			name: "router pending", kube: []runtime.Object{install, certificate, rolledOut},
			grants: []runtime.Object{grant(conditions("True", "Available"))}, reason: "RouterMetadataPending",
		},
	} {
		t.Run(test.name, func(t *testing.T) {
			reconciler := &Reconciler{
				kube:    fake.NewSimpleClientset(test.kube...),
				dynamic: dynamicfake.NewSimpleDynamicClient(runtime.NewScheme(), test.grants...),
			}
			revision := newRevision()
			if err := reconciler.ReconcileKind(ctx, revision); err != nil {
				t.Fatal(err)
			}
			condition := revision.Status.GetCondition(servingv1.RevisionConditionResourcesAvailable)
			if condition == nil || condition.Reason != test.reason {
				t.Fatalf("ResourcesAvailable = %+v, want %s", condition, test.reason)
			}
		})
	}
}
