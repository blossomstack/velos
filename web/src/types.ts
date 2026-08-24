// Wire types as served by the Velos server (camelCase JSON).
// These mirror the fluorite-generated protocol types — the dashboard only reads
// the fields it renders, treating the document as otherwise opaque.

export interface ObjectMeta {
  name: string;
  uid?: string;
  labels?: Record<string, string>;
  annotations?: Record<string, string>;
  resourceVersion?: number;
  creationTimestamp?: string;
  deletionTimestamp?: string;
  finalizers?: string[];
}

export type ContainerPhase =
  | "Pending"
  | "Scheduled"
  | "Running"
  | "Hibernated"
  | "Succeeded"
  | "Failed"
  | "Unknown";

export type RestartPolicy = "Never" | "OnFailure" | "Always";

/// The run state the user asked for (`spec.desiredState`), as opposed to the
/// phase the worker observes (`status.phase`).
export type DesiredState = "Running" | "Hibernated";

export interface ResourceSpec {
  cpu?: number;
  memoryBytes?: number;
}

export type TaintEffect = "NoSchedule" | "PreferNoSchedule";

export type NodeSelectorOperator = "In" | "NotIn" | "Exists" | "DoesNotExist" | "Gt" | "Lt";

export interface NodeSelectorRequirement {
  key: string;
  operator: NodeSelectorOperator;
  values?: string[];
}

export interface NodeSelectorTerm {
  matchExpressions?: NodeSelectorRequirement[];
}

export interface PreferredSchedulingTerm {
  weight: number;
  preference: NodeSelectorTerm;
}

/// Required terms are a hard filter (any one may match); preferred ones only
/// move a worker's score. A container can be `Pending` forever because of the
/// first and never because of the second.
export interface NodeAffinity {
  required?: NodeSelectorTerm[];
  preferred?: PreferredSchedulingTerm[];
}

export interface Taint {
  key: string;
  value?: string;
  effect: TaintEffect;
}

export interface Toleration {
  key: string;
  operator: "Equal" | "Exists";
  value?: string;
  effect?: TaintEffect;
}

export interface ContainerSpec {
  image: string;
  command?: string[];
  env?: Record<string, string>;
  resources?: ResourceSpec;
  restartPolicy?: RestartPolicy;
  desiredState?: DesiredState;
  /// The worker the *user* pinned this container to — a hard filter on
  /// placement, not a record of it. Where the container actually landed is
  /// `status.workerName`.
  nodeName?: string;
  nodeSelector?: Record<string, string>;
  affinity?: NodeAffinity;
  tolerations?: Toleration[];
}

export interface ContainerStatus {
  phase?: ContainerPhase;
  workerName?: string;
  containerID?: string;
  startedAt?: string;
  hibernatedAt?: string;
  finishedAt?: string;
  exitCode?: number;
  /// Why the container is not in the phase the user asked for — e.g.
  /// `StartFailed` when the worker could not launch it. Set alongside
  /// `message`, and cleared by the next status the worker publishes.
  reason?: string;
  message?: string;
}

export interface Container {
  metadata: ObjectMeta;
  spec: ContainerSpec;
  status?: ContainerStatus;
}

export interface Capacity {
  cpu?: number;
  memoryBytes?: number;
}

export interface WorkerCondition {
  conditionType: "Ready";
  status: boolean;
  lastTransitionTime?: string;
  reason?: string;
}

export interface NodeSystemInfo {
  agentVersion?: string;
  os?: string;
  arch?: string;
  hostname?: string;
}

export interface WorkerStatus {
  capacity?: Capacity;
  allocatable?: Capacity;
  conditions?: WorkerCondition[];
  addresses?: string[];
  containerRuntimeVersion?: string;
  nodeInfo?: NodeSystemInfo;
}

export interface WorkerSpec {
  /// Cordoned: the worker keeps running what it has and takes nothing new.
  unschedulable?: boolean;
  taints?: Taint[];
}

export interface Worker {
  metadata: ObjectMeta;
  spec: WorkerSpec;
  status?: WorkerStatus;
}

export interface LeaseSpec {
  holderIdentity?: string;
  renewTime?: string;
  leaseDurationSeconds?: number;
}

export interface Lease {
  metadata: ObjectMeta;
  spec: LeaseSpec;
}

/// One port a Service exposes. Velos has no cluster IP — each worker's
/// container network is its own island — so there is no equivalent of the
/// Kubernetes `port` field: `nodePort` is bound on the workers, and forwarded
/// to `targetPort` inside the container. The server assigns `nodePort` at
/// admission, so a served Service always has one.
export interface ServicePort {
  name?: string;
  targetPort: number;
  nodePort: number;
}

export interface ServiceSpec {
  selector: Record<string, string>;
  ports: ServicePort[];
}

/// One address the service is answering on right now, maintained by the
/// endpoints controller. This is what you point a reverse proxy at.
export interface ServiceEndpoint {
  workerName: string;
  address: string;
  nodePort: number;
  containerName: string;
}

export interface ServiceStatus {
  endpoints?: ServiceEndpoint[];
}

export interface Service {
  metadata: ObjectMeta;
  spec: ServiceSpec;
  status?: ServiceStatus;
}

export interface List<T> {
  items: T[];
}
