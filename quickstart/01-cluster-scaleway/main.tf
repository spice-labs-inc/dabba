terraform {
  required_providers {
    scaleway = {
      source  = "scaleway/scaleway"
      version = ">= 2.39"
    }
    local = {
      source = "hashicorp/local"
    }
  }
}

# NOTE: module source tracks main until the first tagged release, after which
# the quickstart pins a tag so it never breaks underneath a user.
module "cluster" {
  source = "git::https://github.com/spice-labs-inc/dabba-modules.git//modules/scaleway-kapsule?ref=main"

  name        = var.cluster_name
  region      = var.region
  zone        = var.zone
  k8s_version = var.k8s_version

  node_type      = var.node_type
  node_count     = var.node_count
  autoscaling    = var.autoscaling
  max_node_count = var.max_node_count

  # Empty provisions a dedicated private network; set it to join an existing one.
  private_network_id = var.private_network_id
}

variable "cluster_name" {
  type    = string
  default = "dabba"
}

variable "region" {
  type    = string
  default = "fr-par"
}

# Must be inside `region`. A mismatch is rejected at apply, not at plan.
variable "zone" {
  type    = string
  default = "fr-par-1"
}

variable "k8s_version" {
  type    = string
  default = "1.31"
}

variable "node_type" {
  type    = string
  default = "PRO2-XXS"
}

variable "node_count" {
  type    = number
  default = 2
}

variable "autoscaling" {
  type    = bool
  default = false
}

# Also the ceiling on what this cluster can cost.
variable "max_node_count" {
  type    = number
  default = 4
}

variable "private_network_id" {
  type    = string
  default = ""
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
# 02-bootstrap cluster-vars, the same way the eks stage does.
output "region" {
  value = module.cluster.region
}

output "zone" {
  value = module.cluster.zone
}

output "cluster_endpoint" {
  value = module.cluster.cluster_endpoint
}

output "private_network_id" {
  value = module.cluster.private_network_id
}

# Kapsule's per-cluster wildcard, usable for reaching Gateway endpoints before a
# real domain is delegated.
output "wildcard_dns" {
  value = module.cluster.wildcard_dns
}
