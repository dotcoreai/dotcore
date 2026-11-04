# Sourced by the cluster recipes from the repository root; not executed on its own, so it carries no shebang.
#
# A cluster is this workspace's manager site plus workers, each on a target: `local` for this machine, or a
# remote host as `<group>:<host>`. The record beside the manager site lists one target per line, followed by
# the worker's node id once its join completed. The manager only knows its nodes by network address, so the
# record is how the recipes find every worker again.

cluster_record="$DOTCORE_SITE_DIR.cluster"

# Prints a target in its recorded form: `local`, or the remote host a group or host name resolves to.
normalize_target() {
  if test "$1" = local; then
    echo local
  else
    just dev::remote::resolve "$1"
  fi
}

# Prints the recorded targets, one per line.
recorded_targets() {
  if test -f "$cluster_record"; then cut -d' ' -f1 "$cluster_record"; fi
}

# Prints the node id recorded for a target, empty when its join never completed.
recorded_node_id() {
  if test -f "$cluster_record"; then awk -v target="$1" '$1 == target { print $2 }' "$cluster_record"; fi
}

# Records a target before its join, without a node id yet.
record_target() {
  echo "$1" >> "$cluster_record"
}

# Records the node id of a target whose join completed.
record_node_id() {
  awk -v target="$1" -v id="$2" '$1 == target { print target " " id; next } { print }' "$cluster_record" > "$cluster_record.tmp"
  mv "$cluster_record.tmp" "$cluster_record"
}

# Drops a target from the record, and the record once it is empty.
forget_target() {
  test -f "$cluster_record" || return 0
  awk -v target="$1" '$1 != target' "$cluster_record" > "$cluster_record.tmp"
  mv "$cluster_record.tmp" "$cluster_record"
  test -s "$cluster_record" || rm -f "$cluster_record"
}

# Starts a target's worker; a remote start syncs this workspace first.
start_worker() {
  if test "$1" = local; then
    just dev::cluster::_worker-up
  else
    just dev::remote::run "$1" dev::cluster::_worker-up
  fi
}

stop_worker() {
  if test "$1" = local; then
    just dev::cluster::_worker-down
  else
    just dev::remote::exec "$1" just dev::cluster::_worker-down
  fi
}

# Makes a target's stopped worker leave; a remote leave cannot ask, so the calling recipe asks first and the leave runs confirmed.
leave_worker() {
  if test "$1" = local; then
    just dev::cluster::_worker-leave
  else
    just dev::remote::exec "$1" DOTCORE_YES=1 just dev::cluster::_worker-leave
  fi
}
