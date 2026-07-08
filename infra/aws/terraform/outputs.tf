output "ecr_repository_url" {
  description = "ECR repository URL for the solver image."
  value       = aws_ecr_repository.solver.repository_url
}

output "aws_region" {
  description = "AWS region used by this Terraform root."
  value       = var.aws_region
}

output "container_image" {
  description = "Container image URI configured in the ECS task definitions."
  value       = local.container_image
}

output "alb_dns_name" {
  description = "ALB DNS name."
  value       = aws_lb.solver.dns_name
}

output "alb_url" {
  description = "Solver base URL using the configured ALB listener mode."
  value       = var.acm_certificate_arn == null ? "http://${aws_lb.solver.dns_name}" : "https://${aws_lb.solver.dns_name}"
}

output "ecs_cluster_name" {
  description = "ECS cluster name."
  value       = aws_ecs_cluster.solver.name
}

output "ecs_service_name" {
  description = "ECS service name."
  value       = aws_ecs_service.solver.name
}

output "service_task_definition_arn" {
  description = "ECS task definition ARN for the long-running solver service."
  value       = aws_ecs_task_definition.solver.arn
}

output "seed_task_definition_arn" {
  description = "ECS task definition ARN for the one-off seed task."
  value       = try(aws_ecs_task_definition.seed[0].arn, null)
}

output "private_subnet_ids" {
  description = "Private subnet IDs used by ECS and MemoryDB."
  value       = local.private_subnet_ids
}

output "ecs_security_group_id" {
  description = "Security group ID for ECS tasks."
  value       = aws_security_group.ecs.id
}

output "memorydb_endpoint" {
  description = "MemoryDB cluster endpoint."
  value       = "${aws_memorydb_cluster.solver.cluster_endpoint[0].address}:${aws_memorydb_cluster.solver.cluster_endpoint[0].port}"
}

output "redis_url_secret_arn" {
  description = "Secrets Manager ARN containing REDIS_URL when manage_memorydb_auth=true."
  value       = try(aws_secretsmanager_secret.redis_url[0].arn, null)
}

output "bootstrap_config_secret_arn" {
  description = "Secrets Manager ARN expected to contain BOOTSTRAP_CONFIG JSON."
  value       = local.bootstrap_config_secret_arn
}

output "solver_private_key_secret_arn" {
  description = "Secrets Manager ARN for SOLVER_PRIVATE_KEY, when created or provided."
  value       = local.solver_private_key_secret_arn
}

output "jwt_secret_arn" {
  description = "Secrets Manager ARN for JWT_SECRET, when created or provided."
  value       = local.jwt_secret_arn
}

output "task_role_arn" {
  description = "IAM role ARN assumed by the solver application."
  value       = aws_iam_role.task.arn
}

output "execution_role_arn" {
  description = "IAM role ARN used by ECS to pull images, write logs, and fetch secrets."
  value       = aws_iam_role.execution.arn
}

output "cloudwatch_dashboard_name" {
  description = "CloudWatch dashboard name for the solver service."
  value       = try(aws_cloudwatch_dashboard.solver[0].dashboard_name, null)
}
