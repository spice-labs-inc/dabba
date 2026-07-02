terraform {
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = ">= 5.40"
    }
    local = {
      source = "hashicorp/local"
    }
  }
}

# NOTE: module source tracks main until the first tagged release, after which
# the quickstart pins a tag so it never breaks underneath a user.
module "cluster" {
  source = "git::https://github.com/spice-labs-inc/dabba-modules.git//modules/eks-fargate?ref=main"

  name            = var.cluster_name
  region          = var.region
  k8s_version     = var.k8s_version
  route53_zone_id = var.route53_zone_id

  # Empty vpc_id provisions a dedicated VPC; set it (with subnets) to reuse one.
  vpc_id             = var.vpc_id
  private_subnet_ids = var.private_subnet_ids
  public_subnet_ids  = var.public_subnet_ids
}

variable "cluster_name" {
  type    = string
  default = "dabba"
}

variable "region" {
  type    = string
  default = "us-east-1"
}

variable "k8s_version" {
  type    = string
  default = "1.31"
}

# Empty until a Route53 zone is delegated; setting it arms external-dns + cert-manager.
variable "route53_zone_id" {
  type    = string
  default = ""
}

# Empty provisions a dedicated VPC; set vpc_id + both subnet lists to reuse one.
variable "vpc_id" {
  type    = string
  default = ""
}

variable "private_subnet_ids" {
  type    = list(string)
  default = []
}

variable "public_subnet_ids" {
  type    = list(string)
  default = []
}

resource "local_sensitive_file" "kubeconfig" {
  content         = module.cluster.kubeconfig
  filename        = "${path.module}/../kubeconfig"
  file_permission = "0600"
}

output "kubeconfig_path" {
  value = abspath(local_sensitive_file.kubeconfig.filename)
}

# Re-exported for the dabba CLI to read via `tofu output` and thread into the
# 02-bootstrap cluster-vars (IRSA roles, EFS, VPC, region for the cloud overlay).
output "region" {
  value = module.cluster.region
}
output "vpc_id" {
  value = module.cluster.vpc_id
}
output "lb_controller_role_arn" {
  value = module.cluster.lb_controller_role_arn
}
output "external_dns_role_arn" {
  value = module.cluster.external_dns_role_arn
}
output "cert_manager_role_arn" {
  value = module.cluster.cert_manager_role_arn
}
