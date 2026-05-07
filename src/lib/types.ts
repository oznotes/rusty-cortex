export type ProtocolType = "Fastboot" | "Adb" | "Edl" | "MtkBrom";
export type AdbState = "Normal" | "Recovery" | "Sideload";
export type RebootMode = "Normal" | "Bootloader" | "Recovery" | "Edl";

export interface DeviceInfo {
  vendor_id: number;
  product_id: number;
  serial: string | null;
  manufacturer: string | null;
  product: string | null;
  protocol: ProtocolType;
  adb_state?: AdbState | null;
}

export type FlashStage =
  | "Idle"
  | "Validating"
  | "Sending"
  | "Flashing"
  | "Complete"
  | "Error";

export interface FlashProgress {
  stage: FlashStage;
  message: string;
  percent?: number;
}

export interface LogEntry {
  timestamp: Date;
  message: string;
  level: "info" | "warn" | "error";
}

export type ShellOutput =
  | { kind: "Data"; data: number[] }
  | { kind: "Stderr"; data: number[] }
  | { kind: "Exit"; message: string; code?: number };

export type RootType = "None" | "Adb" | "Su";

export interface RootStatus {
  root_type: RootType;
  message: string;
}

export interface PartitionInfo {
  name: string;
  size_bytes: number | null;
  size_display: string;
}

export interface DumpListResult {
  partitions: PartitionInfo[];
  temp_dir: string;
  free_bytes: number | null;
  supports_shell_v2: boolean;
}

export interface DeviceHealth {
  battery_level: number | null;
  battery_health: string | null;
  battery_temp: number | null;
  storage_used_gb: number | null;
  storage_total_gb: number | null;
  ram_used_gb: number | null;
  ram_total_gb: number | null;
}

export interface EdlDeviceInfo {
  serial: string | null;
  hw_id: string | null;
  pk_hash: string | null;
  storage_type: string | null;
  sector_size: number | null;
  num_luns: number | null;
  firehose_active: boolean;
  chipset: string | null;
}

export interface EdlPartitionEntry {
  name: string;
  start_sector: number;
  num_sectors: number;
  size_bytes: number;
  lun: number;
  type_guid: string;
  unique_guid: string;
  attributes: number;
  category: string;
}

export interface BatchFlashResult {
  programmed: string[];
  erased: string[];
  patched: number;
  errors: string[];
  verified: [string, boolean][];
}

export interface ProgrammerEntry {
  programmer_path: string;
  programmer_name: string;
  device_serial: string | null;
  storage_type: string | null;
  last_used: string;
  use_count: number;
  file_exists: boolean;
}

export type MatchLevel = "BinaryVerified" | "DbExact" | "FilenameMatch" | "Unknown" | "DbOtherDevice";

export interface ProgrammerCandidate {
  name: string;
  path: string;
  valid: boolean;
  size_bytes: number;
  match_level: MatchLevel;
  identity: ProgrammerIdentity | null;
}

export interface ProgrammerIdentity {
  hw_id: string;
  pk_hash: string;
  hash_algorithm: "Sha256" | "Sha384";
  msm_id: number;
  oem_id: number;
  model_id: number;
  chipset: string | null;
  hwid_from_filename: boolean;
}

export interface VerifyResult {
  passed: boolean;
  bytes_checked: number;
  detail: string;
}
