# Sourced by the site and cluster recipes; not executed on its own, so it carries no shebang.
#
# Recipes take a node's inference and applications choices as comma-separated words, such as
# `docker,simulated`, while dotc takes one flag per value.

# Sets `choice_flags` to one --inference flag per inference choice and one --applications flag per
# applications choice, ignoring spaces and empty entries, and `native_flags` to an ephemeral native
# workload lifetime when apple is among the inference choices, so native workloads stop with the
# development server. An inference choice such as `simulated:rtx-5090` passes through as is.
site_choice_flags() {
  local inference="${1// /}" applications="${2// /}" value values
  choice_flags=()
  native_flags=()
  IFS=, read -ra values <<< "$inference"
  for value in "${values[@]}"; do
    if [ -n "$value" ]; then choice_flags+=(--inference "$value"); fi
  done
  IFS=, read -ra values <<< "$applications"
  for value in "${values[@]}"; do
    if [ -n "$value" ]; then choice_flags+=(--applications "$value"); fi
  done
  case ",$inference," in
    *,apple,*) native_flags=(--native-workload-lifetime ephemeral) ;;
  esac
}
