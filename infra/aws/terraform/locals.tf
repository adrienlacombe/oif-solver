data "aws_availability_zones" "available" {
  state = "available"
}

locals {
  raw_name = trim(replace(lower("${var.project}-${var.environment}"), "/[^a-z0-9-]/", "-"), "-")
  name     = substr(local.raw_name, 0, min(32, length(local.raw_name)))

  tags = merge(
    {
      Project     = var.project
      Environment = var.environment
      ManagedBy   = "terraform"
    },
    var.tags
  )

  azs        = length(var.availability_zones) > 0 ? var.availability_zones : slice(data.aws_availability_zones.available.names, 0, var.az_count)
  create_vpc = var.vpc_id == null || var.vpc_id == ""

  vpc_id             = local.create_vpc ? aws_vpc.this[0].id : var.vpc_id
  public_subnet_ids  = local.create_vpc ? aws_subnet.public[*].id : var.public_subnet_ids
  private_subnet_ids = local.create_vpc ? aws_subnet.private[*].id : var.private_subnet_ids
  alb_subnet_ids     = var.alb_internal ? local.private_subnet_ids : local.public_subnet_ids

  container_image = coalesce(var.container_image, "${aws_ecr_repository.solver.repository_url}:${var.image_tag}")

  memorydb_user_name_raw = "solver${replace(local.name, "-", "")}"
  memorydb_user_name     = substr(local.memorydb_user_name_raw, 0, min(40, length(local.memorydb_user_name_raw)))
  memorydb_scheme        = var.memorydb_tls_enabled ? "rediss" : "redis"
  memorydb_acl_name      = var.manage_memorydb_auth ? aws_memorydb_acl.solver[0].name : "open-access"
  memorydb_auth_prefix   = var.manage_memorydb_auth ? "${local.memorydb_user_name}:${random_password.memorydb[0].result}@" : ""
  memorydb_url           = "${local.memorydb_scheme}://${local.memorydb_auth_prefix}${aws_memorydb_cluster.solver.cluster_endpoint[0].address}:${aws_memorydb_cluster.solver.cluster_endpoint[0].port}"

  bootstrap_config_secret_arn   = var.bootstrap_config_secret_arn != null ? var.bootstrap_config_secret_arn : try(aws_secretsmanager_secret.bootstrap_config[0].arn, null)
  solver_private_key_secret_arn = var.solver_private_key_secret_arn != null ? var.solver_private_key_secret_arn : try(aws_secretsmanager_secret.solver_private_key[0].arn, null)
  jwt_secret_arn                = var.jwt_secret_arn != null ? var.jwt_secret_arn : try(aws_secretsmanager_secret.jwt[0].arn, null)

  base_environment = merge(
    {
      AWS_DEFAULT_REGION  = var.aws_region
      AWS_REGION          = var.aws_region
      REDIS_CLUSTER_MODE  = "true"
      REQUIRE_PERSISTENCE = tostring(var.require_persistence)
      RUST_LOG            = var.rust_log
      SOLVER_API_PORT     = tostring(var.container_port)
      SOLVER_ID           = var.solver_id
      STORAGE_BACKEND     = "redis"
    },
    var.extra_environment
  )

  plaintext_environment_map = merge(
    local.base_environment,
    var.manage_memorydb_auth ? tomap({}) : tomap({ REDIS_URL = local.memorydb_url })
  )

  service_environment = [
    for key in sort(keys(local.plaintext_environment_map)) : {
      name  = key
      value = local.plaintext_environment_map[key]
    }
  ]

  service_secrets_map = merge(
    var.manage_memorydb_auth ? tomap({ REDIS_URL = aws_secretsmanager_secret.redis_url[0].arn }) : tomap({}),
    local.solver_private_key_secret_arn == null ? tomap({}) : tomap({ SOLVER_PRIVATE_KEY = local.solver_private_key_secret_arn }),
    local.jwt_secret_arn == null ? tomap({}) : tomap({ JWT_SECRET = local.jwt_secret_arn }),
    var.extra_secrets
  )

  service_secrets = [
    for key in sort(keys(local.service_secrets_map)) : {
      name      = key
      valueFrom = local.service_secrets_map[key]
    }
  ]

  seed_secrets_map = merge(
    local.service_secrets_map,
    local.bootstrap_config_secret_arn == null ? tomap({}) : tomap({ BOOTSTRAP_CONFIG = local.bootstrap_config_secret_arn })
  )

  seed_secrets = [
    for key in sort(keys(local.seed_secrets_map)) : {
      name      = key
      valueFrom = local.seed_secrets_map[key]
    }
  ]

  execution_secret_arns = distinct(compact(concat(
    values(local.service_secrets_map),
    local.bootstrap_config_secret_arn == null ? [] : [local.bootstrap_config_secret_arn]
  )))

  seed_preset_arg = trimspace(var.seed_preset) == "" ? "" : "--seed ${var.seed_preset}"
  force_seed_arg  = var.force_seed ? "--force-seed" : ""

  seed_shell_command = <<-EOT
    set -eu
    tmp=/tmp/solver-seed.log
    : > "$tmp"
    /app/solver ${local.seed_preset_arg} --bootstrap-config "$BOOTSTRAP_CONFIG" ${local.force_seed_arg} --log-level ${var.solver_log_level} > "$tmp" 2>&1 &
    pid=$!
    tail -f "$tmp" &
    tail_pid=$!
    trap 'kill "$tail_pid" 2>/dev/null || true; kill "$pid" 2>/dev/null || true' EXIT
    while kill -0 "$pid" 2>/dev/null; do
      if grep -Eq "Configuration seeded successfully|Configuration already exists" "$tmp"; then
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
        exit 0
      fi
      sleep 2
    done
    wait "$pid"
  EOT
}
