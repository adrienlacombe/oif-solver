resource "aws_secretsmanager_secret" "redis_url" {
  count = var.manage_memorydb_auth ? 1 : 0

  name                    = "${local.name}/redis-url"
  description             = "Redis URL for ${local.name}, including MemoryDB ACL credentials."
  recovery_window_in_days = var.secrets_recovery_window_in_days
}

resource "aws_secretsmanager_secret_version" "redis_url" {
  count = var.manage_memorydb_auth ? 1 : 0

  secret_id     = aws_secretsmanager_secret.redis_url[0].id
  secret_string = local.memorydb_url
}

resource "aws_secretsmanager_secret" "bootstrap_config" {
  count = var.bootstrap_config_secret_arn == null && var.create_bootstrap_config_secret ? 1 : 0

  name                    = "${local.name}/bootstrap-config"
  description             = "Bootstrap config JSON for the OIF solver seed task."
  recovery_window_in_days = var.secrets_recovery_window_in_days
}

resource "aws_secretsmanager_secret" "solver_private_key" {
  count = var.solver_private_key_secret_arn == null && var.create_solver_private_key_secret ? 1 : 0

  name                    = "${local.name}/solver-private-key"
  description             = "SOLVER_PRIVATE_KEY for local wallet deployments."
  recovery_window_in_days = var.secrets_recovery_window_in_days
}

resource "aws_secretsmanager_secret" "jwt" {
  count = var.jwt_secret_arn == null && var.create_jwt_secret ? 1 : 0

  name                    = "${local.name}/jwt-secret"
  description             = "JWT_SECRET for OIF solver API auth."
  recovery_window_in_days = var.secrets_recovery_window_in_days
}
