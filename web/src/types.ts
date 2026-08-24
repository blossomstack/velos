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

export interface ContainerSpec {
  image: string;
  command?: string[];
  env?: Record<string, string>;
  resources?: ResourceSpec;
  restartPolicy?: RestartPolicy;
  desiredState?: DesiredState;
  nodeName?: string;
}

export interface ContainerStatus {
  phase?: ContainerPhase;
  workerName?: string;
  containerID?: string;
  startedAt?: string;
  hibernatedAt?: string;
  finishedAt?: string;
  exitCode?: number;
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

export interface Worker {
  metadata: ObjectMeta;
  spec: { unschedulable?: boolean };
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
