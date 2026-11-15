# How MO Compares

## Introduction

MO is a small Rust daemon for one host. It manages scarce local resources, such as CPU cores, memory, and virtual desktops, for cooperative AI agents and jobs running on that same host.

Agents request leases through a Unix socket. MO keeps one queue, grants leases when capacity allows, requires heartbeats, and reclaims leases that expire or go stale. All state lives in one SQLite file. MO opens no TCP listener, runs no network API, and contains no LLM.

This niche is narrow by design. Most scheduling and desktop products target clusters, clouds, or fleets of user desktops. MO targets a single machine where several agents must share limited capacity without stepping on each other, and where the machine may run on battery.

None of the 11 combines single-host operation, a battery gate, and virtual-desktop/CDP/VNC leases for AI agents in one daemon.

MO is at version 0.1.0. It is licensed under the PolyForm Noncommercial License 1.0.0. Personal and noncommercial use is free. Commercial use needs a separate license from Movahedi Solutions.

## Comparison table

| Product | License type | Scale target | Job queue and resource leases | Battery awareness | Virtual desktop / CDP / VNC leases | Single-host agent focus |
| --- | --- | --- | --- | --- | --- | --- |
| MO | PolyForm Noncommercial 1.0.0 | Single host | Yes. One queue, heartbeat leases, reclaim on expiry or stale heartbeat | Yes. Refuses new leases at 5 percent or less and not charging | Yes, on hosts that have desktops, through the host vd scripts | Yes |
| Slurm | GPLv2 | Clusters and HPC systems | Yes. Batch job queue and node allocations | No | No | No |
| HTCondor | Apache License 2.0 | Machine pools and distributed clusters | Yes. Job queue with matchmaking between jobs and machines | No | No | No |
| OpenPBS | AGPLv3 | Clusters and HPC systems | Yes. Batch job queue and node allocations | No | No | No |
| Kueue | Apache License 2.0 | Kubernetes clusters | Yes. Job queueing, quota, and admission for Kubernetes workloads | No | No | No |
| Volcano | Apache License 2.0 | Kubernetes clusters | Yes. Batch scheduling for Kubernetes workloads | No | No | No |
| Proxmox VE | AGPLv3 | Virtualization hosts and host clusters | No. Hypervisor management, not a job lease queue | No | Partial. Hosts KVM and LXC guests that can be reached by VNC or SPICE consoles, not CDP or VNC leases for agents | No |
| Apache Guacamole | Apache License 2.0 | Remote desktop access for user fleets | No. Connection gateway, not a job queue | No | Partial. Provides clientless RDP, VNC, and SSH access sessions, not resource leases | No |
| IBM Spectrum LSF | Commercial proprietary | Clusters and HPC systems | Yes. Batch job queue and host allocations | No | No | No |
| Altair PBS Professional | Commercial proprietary | Clusters and HPC systems | Yes. Batch job queue and node allocations | No | No | No |
| HashiCorp Nomad Enterprise | Commercial proprietary | Clusters of hosts, services and batch jobs | Yes. Job scheduling and allocations across a cluster | No | No | No |
| Leostream | Commercial proprietary | VDI estates and hosted desktops | No. Desktop connection brokering, not a job lease queue | No | Partial. Brokers connections to desktops over protocols such as RDP, not CDP leases for agents | No |

## Open source batch schedulers

### Slurm

Slurm is an open source workload manager for Linux clusters, widely used in high performance computing. It queues batch jobs, allocates cluster nodes to those jobs, and tracks job state across many machines. Its design center is the multi-host cluster, often with shared storage and many compute nodes. It has no concept of leasing a virtual desktop or a Chrome DevTools Protocol endpoint to an agent on one laptop class host.

### HTCondor

HTCondor comes from the University of Wisconsin, Madison. It is known for idle-cycle scavenging, where jobs run on machines that would otherwise sit idle, and for matchmaking, where job requirements and machine offers are paired through classified advertisements. HTCondor pools can span desktops, servers, and clusters. It schedules jobs onto machines, but it does not manage virtual desktops, CDP endpoints, or battery state on a single host.

### OpenPBS

OpenPBS is the open source member of the PBS family. It provides batch job queueing and scheduling for clusters, with queues, resource requests, and job accounting. Sites use it to share HPC clusters among many users and groups. Like Slurm and HTCondor, it assumes a pool of compute resources rather than one host with a battery and a small set of virtual desktops.

## Kubernetes batch

### Kueue

Kueue is a Kubernetes native system for job queueing, quota, and admission. It decides when a Kubernetes Job or similar workload may start, based on quotas assigned to tenants and the resources available in the cluster. Kueue needs a running Kubernetes cluster and works through the Kubernetes API. It does not run as a standalone single-host daemon, and it has no battery gate or desktop leasing.

### Volcano

Volcano is a batch scheduler built for Kubernetes. It adds features that batch and high performance workloads often need on Kubernetes, such as gang scheduling, where a set of pods starts together or not at all. Volcano runs inside a Kubernetes cluster and schedules pods across cluster nodes. It does not address single-host agent work, battery state, or virtual desktop leases.

## Desktop layer

Proxmox VE and Apache Guacamole are separate products. They solve different problems, and neither one is a job scheduler. They appear in this comparison because teams building agent desktops often consider a hypervisor or a remote access gateway as part of the stack.

### Proxmox VE

Proxmox VE is a virtualization platform based on Debian Linux. It runs KVM virtual machines and LXC containers, and it provides web based management, storage, and clustering for hosts. An operator can create a desktop virtual machine on Proxmox VE and reach its console. Proxmox VE does not queue agent jobs, grant timed resource leases with heartbeats, or gate work on battery charge.

### Apache Guacamole

Apache Guacamole is a clientless remote desktop gateway. A user connects through a web browser, and Guacamole translates that session to RDP, VNC, or SSH on the target machine. It centralizes remote access and can record sessions and control who reaches which machine. It does not schedule jobs, hold a resource queue, or decide whether a host has enough battery or memory for new work.

## Commercial products

### IBM Spectrum LSF

IBM Spectrum LSF is a commercial workload scheduler with a long history in HPC and electronic design automation. It queues and dispatches batch jobs across clusters according to site policies, limits, and priorities, and it is sold and supported by IBM. LSF targets shared clusters run by IT organizations. It has no single-host agent mode, no battery gate, and no virtual desktop or CDP leasing.

### Altair PBS Professional

Altair PBS Professional is the commercial member of the PBS family, sold and supported by Altair. It provides batch scheduling for HPC clusters, with commercial support and features aimed at large sites. OpenPBS and PBS Professional share PBS heritage, but they are separate products with separate licenses. PBS Professional does not manage desktops for agents on one host, and it does not gate leases on battery state.

### HashiCorp Nomad Enterprise

Nomad is a workload orchestrator from HashiCorp that schedules services, batch jobs, and system jobs across a cluster of hosts. Nomad Enterprise is the commercial edition, and it adds governance and operational features for larger deployments. Nomad Community Edition is licensed under the Business Source License, which is not an OSI open source license. Nomad expects a cluster of servers, and it has no battery awareness or virtual desktop and CDP lease model.

### Leostream

Leostream is a commercial VDI connection broker. It connects users to desktops and applications hosted in data centers or clouds, and it manages pools, policies, and access for desktop estates. Leostream sells to organizations, and minimum orders apply. It brokers user desktop connections. It does not provide a job queue or timed resource leases for agents, and it does not watch host battery state.

## Honest limits of MO

MO has clear limits. A buyer or user should weigh them before choosing it.

- No multi-host coordination. MO manages one host only. It cannot balance work across machines, and it has no cluster state.
- No network API. Clients talk to MO over a Unix socket on the same host. There is no TCP listener, no HTTP server, and no remote access path.
- No GPU scheduling. MO accounts for CPU cores, memory, disk space, and desktop slots. It does not schedule or partition GPUs.
- Young project. MO is at version 0.1.0. The core is in daily use on its reference host, but the project has a short public history and a small user base.
- Linux only. MO depends on Linux facilities such as /proc, Unix sockets, and battery data from sysfs. There is no Windows or macOS version.
- ARM64 cross-build blocked. The x86_64 build is the primary build. The ARM64 cross-build is currently blocked on a missing cross linker on the documented build hosts.
- Desktops only on hosts with the vd scripts. Virtual desktop, CDP, and VNC features work only on hosts that provide the expected vd start and stop scripts and desktop setup. On a generic host, MO leases CPU and memory only.
- Cooperative agents only. MO can detect and audit work that bypasses it, but it cannot stop a process started outside its control from using host resources.
- One desktop per request. A lease request can ask for zero or one desktop. Multi-desktop reservation in a single request is not implemented.

## When MO fits, and when a rival fits better

MO fits when several AI agents or automated jobs share one Linux host, and that host has real limits: a small memory budget, a few virtual desktops with CDP or VNC endpoints, or a battery that must not run flat. It also fits when an operator wants an auditable local queue with heartbeats and reclaim, without running Kubernetes or a cluster scheduler to get it. A single developer workstation, a Chromebook style VM, or a small dedicated agent box are typical cases.

A rival fits better in most other cases. Choose Slurm, HTCondor, OpenPBS, IBM Spectrum LSF, or Altair PBS Professional for multi-host batch work and HPC clusters. Choose Kueue or Volcano when the workloads already run on Kubernetes and the need is quota, admission, or batch scheduling inside that cluster. Choose HashiCorp Nomad Enterprise for orchestrating services and batch jobs across many hosts. Choose Proxmox VE to run and manage virtual machines, and Apache Guacamole to give people browser based remote access to machines. Choose Leostream to broker VDI desktops for an organization.

For paid implementation and support, contact Movahedi Solutions. The money path for MO is paid implementation and support through Movahedi Solutions, not license sales to the general public. Commercial use of MO itself needs a separate license from Movahedi Solutions, as stated in the license section of the README.
