variable "aws_region" {
  description = "AWS region for all regional resources."
  type        = string
  default     = "us-east-1"
}

variable "project" {
  description = "Project name used in resource names and tags."
  type        = string
  default     = "oif-solver"
}

variable "environment" {
  description = "Deployment environment name used in resource names and tags."
  type        = string
  default     = "dev"
}

variable "tags" {
  description = "Additional tags applied to all supported resources."
  type        = map(string)
  default     = {}
}

variable "solver_id" {
  description = "Solver ID to load from storage after seeding. Keep this equal to bootstrap_config.solver_id."
  type        = string
}

variable "container_image" {
  description = "Full container image URI. If null, the created ECR repository plus image_tag is used."
  type        = string
  default     = null
}

variable "image_tag" {
  description = "Image tag used when container_image is null."
  type        = string
  default     = "latest"
}

variable "container_port" {
  description = "Solver API port inside the container."
  type        = number
  default     = 3000
}

variable "rust_log" {
  description = "RUST_LOG value for solver tasks."
  type        = string
  default     = "info"
}

variable "solver_log_level" {
  description = "Value passed to the solver --log-level flag."
  type        = string
  default     = "info"
}

variable "service_desired_count" {
  description = "Desired ECS service task count. Use 0 for the initial infra apply before seeding."
  type        = number
  default     = 0

  validation {
    condition     = var.service_desired_count >= 0
    error_message = "service_desired_count must be non-negative."
  }
}

variable "task_cpu" {
  description = "Fargate task CPU units."
  type        = number
  default     = 512
}

variable "task_memory" {
  description = "Fargate task memory in MiB."
  type        = number
  default     = 1024
}

variable "cpu_architecture" {
  description = "Fargate CPU architecture. Build and push the image for the same platform."
  type        = string
  default     = "ARM64"

  validation {
    condition     = contains(["ARM64", "X86_64"], var.cpu_architecture)
    error_message = "cpu_architecture must be ARM64 or X86_64."
  }
}

variable "enable_execute_command" {
  description = "Enable ECS Exec on the solver service."
  type        = bool
  default     = false
}

variable "container_insights" {
  description = "Enable ECS Container Insights for the cluster."
  type        = bool
  default     = true
}

variable "extra_environment" {
  description = "Additional non-secret environment variables for service and seed tasks."
  type        = map(string)
  default     = {}
}

variable "extra_secrets" {
  description = "Additional ECS secret environment variables. Map of ENV_NAME to Secrets Manager ARN."
  type        = map(string)
  default     = {}
}

variable "require_persistence" {
  description = "Sets REQUIRE_PERSISTENCE for storage readiness checks."
  type        = bool
  default     = false
}

variable "vpc_id" {
  description = "Existing VPC ID. Leave null to create a dedicated VPC."
  type        = string
  default     = null
}

variable "public_subnet_ids" {
  description = "Existing public subnet IDs for the ALB when vpc_id is set."
  type        = list(string)
  default     = []
}

variable "private_subnet_ids" {
  description = "Existing private subnet IDs for ECS and MemoryDB when vpc_id is set."
  type        = list(string)
  default     = []
}

variable "vpc_cidr" {
  description = "CIDR block for the created VPC."
  type        = string
  default     = "10.42.0.0/16"
}

variable "az_count" {
  description = "Number of availability zones to use when creating a VPC."
  type        = number
  default     = 2

  validation {
    condition     = var.az_count >= 2 && var.az_count <= 3
    error_message = "az_count must be 2 or 3."
  }
}

variable "availability_zones" {
  description = "Explicit availability zones for created subnets. Leave empty to use the first az_count available AZs."
  type        = list(string)
  default     = []
}

variable "create_nat_gateway" {
  description = "Create NAT gateway access for private ECS tasks."
  type        = bool
  default     = true
}

variable "single_nat_gateway" {
  description = "Use one shared NAT gateway instead of one per AZ."
  type        = bool
  default     = true
}

variable "alb_internal" {
  description = "Create an internal ALB instead of an internet-facing ALB."
  type        = bool
  default     = false
}

variable "alb_allowed_cidrs" {
  description = "CIDR blocks allowed to reach the ALB."
  type        = list(string)
  default     = ["0.0.0.0/0"]
}

variable "acm_certificate_arn" {
  description = "ACM certificate ARN. If set, HTTPS is enabled and HTTP redirects to HTTPS."
  type        = string
  default     = null
}

variable "alb_deletion_protection" {
  description = "Enable deletion protection on the ALB."
  type        = bool
  default     = false
}

variable "health_check_path" {
  description = "ALB target group health check path."
  type        = string
  default     = "/health"
}

variable "health_check_grace_period_seconds" {
  description = "ECS service health check grace period."
  type        = number
  default     = 120
}

variable "memorydb_node_type" {
  description = "MemoryDB node type."
  type        = string
  default     = "db.t4g.small"
}

variable "memorydb_num_shards" {
  description = "Number of MemoryDB shards. Keep 1 unless the solver storage code has been tested for multi-shard key distribution."
  type        = number
  default     = 1
}

variable "memorydb_num_replicas_per_shard" {
  description = "Number of MemoryDB replicas per shard."
  type        = number
  default     = 1
}

variable "memorydb_engine_version" {
  description = "MemoryDB Redis OSS engine version."
  type        = string
  default     = "7.0"
}

variable "memorydb_parameter_group_name" {
  description = "MemoryDB parameter group name."
  type        = string
  default     = "default.memorydb-redis7"
}

variable "memorydb_snapshot_retention_limit" {
  description = "Number of days to retain MemoryDB snapshots."
  type        = number
  default     = 7
}

variable "memorydb_maintenance_window" {
  description = "Preferred MemoryDB maintenance window."
  type        = string
  default     = "sun:05:00-sun:06:00"
}

variable "memorydb_tls_enabled" {
  description = "Enable in-transit TLS for MemoryDB. The solver uses rediss:// when true."
  type        = bool
  default     = true
}

variable "manage_memorydb_auth" {
  description = "Create a MemoryDB ACL user/password and pass REDIS_URL through Secrets Manager."
  type        = bool
  default     = true
}

variable "seed_preset" {
  description = "Seed preset for the seed task. Use an empty string for seedless mode."
  type        = string
  default     = "testnet"
}

variable "force_seed" {
  description = "Pass --force-seed to the seed task."
  type        = bool
  default     = false
}

variable "create_bootstrap_config_secret" {
  description = "Create a placeholder Secrets Manager secret for BOOTSTRAP_CONFIG."
  type        = bool
  default     = true
}

variable "bootstrap_config_secret_arn" {
  description = "Existing Secrets Manager ARN containing bootstrap config JSON. Overrides create_bootstrap_config_secret."
  type        = string
  default     = null
}

variable "create_solver_private_key_secret" {
  description = "Create a placeholder secret for SOLVER_PRIVATE_KEY. Use only for local wallet deployments."
  type        = bool
  default     = false
}

variable "solver_private_key_secret_arn" {
  description = "Existing Secrets Manager ARN for SOLVER_PRIVATE_KEY. Not needed for KMS signer deployments."
  type        = string
  default     = null
}

variable "create_jwt_secret" {
  description = "Create a placeholder secret for JWT_SECRET."
  type        = bool
  default     = false
}

variable "jwt_secret_arn" {
  description = "Existing Secrets Manager ARN for JWT_SECRET. Required when admin or orders auth is enabled."
  type        = string
  default     = null
}

variable "secret_kms_key_arns" {
  description = "KMS key ARNs needed to decrypt custom Secrets Manager secrets."
  type        = list(string)
  default     = []
}

variable "kms_key_arns" {
  description = "AWS KMS signing key ARNs the solver task role may use for KMS-backed wallets."
  type        = list(string)
  default     = []
}

variable "secrets_recovery_window_in_days" {
  description = "Recovery window for Secrets Manager secrets created by this Terraform root."
  type        = number
  default     = 7
}

variable "log_retention_days" {
  description = "CloudWatch log retention for solver task logs."
  type        = number
  default     = 30
}
