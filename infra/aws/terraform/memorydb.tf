resource "random_password" "memorydb" {
  count = var.manage_memorydb_auth ? 1 : 0

  length  = 32
  special = false
}

resource "aws_memorydb_user" "solver" {
  count = var.manage_memorydb_auth ? 1 : 0

  user_name     = local.memorydb_user_name
  access_string = "on ~* &* +@all"

  authentication_mode {
    type      = "password"
    passwords = [random_password.memorydb[0].result]
  }
}

resource "aws_memorydb_acl" "solver" {
  count = var.manage_memorydb_auth ? 1 : 0

  name       = local.name
  user_names = [aws_memorydb_user.solver[0].user_name]
}

resource "aws_memorydb_subnet_group" "solver" {
  name       = local.name
  subnet_ids = local.private_subnet_ids
}

resource "aws_memorydb_cluster" "solver" {
  name                       = local.name
  acl_name                   = local.memorydb_acl_name
  node_type                  = var.memorydb_node_type
  num_shards                 = var.memorydb_num_shards
  num_replicas_per_shard     = var.memorydb_num_replicas_per_shard
  engine_version             = var.memorydb_engine_version
  parameter_group_name       = var.memorydb_parameter_group_name
  port                       = 6379
  security_group_ids         = [aws_security_group.memorydb.id]
  snapshot_retention_limit   = var.memorydb_snapshot_retention_limit
  subnet_group_name          = aws_memorydb_subnet_group.solver.name
  tls_enabled                = var.memorydb_tls_enabled
  maintenance_window         = var.memorydb_maintenance_window
  auto_minor_version_upgrade = true

  lifecycle {
    precondition {
      condition     = var.memorydb_tls_enabled || !var.manage_memorydb_auth
      error_message = "MemoryDB auth requires TLS. Set memorydb_tls_enabled=true or manage_memorydb_auth=false."
    }
  }
}
