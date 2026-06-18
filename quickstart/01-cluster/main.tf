terraform {
  required_providers {
    kind = {
      source  = "tehcyx/kind"
      version = "~> 0.8"
    }
    local = {
      source = "hashicorp/local"
    }
  }
}

# NOTE: module sources track main until the first tagged release, after which
# the quickstart pins a tag so it never breaks underneath a user.
module "cluster" {
  source = "git::https://github.com/spice-labs-inc/dabba-modules.git//modules/kind?ref=main"

  name             = var.cluster_name
  registry_mirrors = var.registry_mirrors
}

# Empty by default. The local-test harness sets TF_VAR_registry_mirrors to point
# kind at a pull-through cache so repeat runs skip re-pulling images.
variable "registry_mirrors" {
  type    = map(string)
  default = {}
}

resource "local_sensitive_file" "kubeconfig" {
  content         = module.cluster.kubeconfig
  filename        = "${path.module}/../kubeconfig"
  file_permission = "0600"
}

variable "cluster_name" {
  type    = string
  default = "dabba"
}

output "kubeconfig_path" {
  value = abspath(local_sensitive_file.kubeconfig.filename)
}
