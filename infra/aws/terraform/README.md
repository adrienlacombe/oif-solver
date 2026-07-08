# AWS Terraform Deployment

This Terraform root provisions the AWS runtime for the `solver` binary:

- ECR repository for the Docker image
- VPC, public/private subnets, NAT, and security groups unless an existing VPC is supplied
- Public or internal ALB with `/health` checks
- ECS Fargate cluster, long-running service task definition, and one-off seed task definition
- MemoryDB for Redis in cluster mode
- CloudWatch logs
- IAM roles for ECS execution, optional Secrets Manager decrypt, and optional KMS signing
- Secrets Manager wiring for `REDIS_URL`, `BOOTSTRAP_CONFIG`, `JWT_SECRET`, and `SOLVER_PRIVATE_KEY`

The solver stores runtime `OperatorConfig` in Redis-compatible storage under the
`${SOLVER_ID}-operator` key. A fresh deployment must seed that config before
the ECS service is scaled above zero.

## First Deploy

```bash
cd infra/aws/terraform
cp terraform.tfvars.example terraform.tfvars
terraform init
terraform apply
```

Build and push the image after ECR exists. Use the same platform as
`cpu_architecture` in Terraform. For KMS signer support, include `FEATURES=kms`.

```bash
aws ecr get-login-password --region "$(terraform output -raw aws_region 2>/dev/null || echo us-east-1)" \
  | docker login --username AWS --password-stdin "$(terraform output -raw ecr_repository_url | cut -d/ -f1)"

docker buildx build \
  --platform linux/arm64 \
  --build-arg FEATURES=kms \
  -t "$(terraform output -raw container_image)" \
  --push \
  ../../..
```

Use `--platform linux/amd64` instead if `cpu_architecture = "X86_64"`.

Fill the placeholder secrets. `BOOTSTRAP_CONFIG` can be based on
`examples/bootstrap-testnet-kms.json`; update the `solver_id`, admin domain,
admin address, KMS key ARN, and any RPC/token overrides first.

```bash
aws secretsmanager put-secret-value \
  --secret-id "$(terraform output -raw bootstrap_config_secret_arn)" \
  --secret-string "$(jq -c . examples/bootstrap-testnet-kms.json)"

aws secretsmanager put-secret-value \
  --secret-id "$(terraform output -raw jwt_secret_arn)" \
  --secret-string "$(openssl rand -base64 48)"
```

For a local wallet deployment, set `create_solver_private_key_secret = true`,
remove the KMS account block from the bootstrap config, and fill the
`SOLVER_PRIVATE_KEY` secret:

```bash
aws secretsmanager put-secret-value \
  --secret-id "$(terraform output -raw solver_private_key_secret_arn)" \
  --secret-string "0x..."
```

Run the one-off seed task in the private subnets. It exits after the solver logs
that configuration was seeded or already existed.

```bash
SUBNETS=$(terraform output -json private_subnet_ids | jq -r 'join(",")')
SG=$(terraform output -raw ecs_security_group_id)

aws ecs run-task \
  --cluster "$(terraform output -raw ecs_cluster_name)" \
  --task-definition "$(terraform output -raw seed_task_definition_arn)" \
  --launch-type FARGATE \
  --network-configuration "awsvpcConfiguration={subnets=[$SUBNETS],securityGroups=[$SG],assignPublicIp=DISABLED}"
```

After the seed task succeeds, scale the service:

```hcl
service_desired_count = 1
```

```bash
terraform apply
curl "$(terraform output -raw alb_url)/health"
```

## Existing VPC

Set `vpc_id`, `public_subnet_ids`, and `private_subnet_ids`. Leave the created
VPC variables at their defaults. Private subnets must have outbound internet
access for ECR pulls, CloudWatch logs, Secrets Manager, and chain RPC endpoints,
unless you provide equivalent VPC endpoints plus RPC access.

## Secrets and State

Terraform creates a generated MemoryDB password when
`manage_memorydb_auth = true` and stores the resulting `REDIS_URL` in Secrets
Manager for ECS injection. The generated password still exists in Terraform
state because MemoryDB requires the password at resource creation time. Store
Terraform state in a locked, encrypted backend such as S3 with DynamoDB locking.

Do not put solver private keys, JWT secrets, or bootstrap JSON directly in
`*.tfvars`; pass existing Secrets Manager ARNs or fill the placeholder secrets
after `terraform apply`.
