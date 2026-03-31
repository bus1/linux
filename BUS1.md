# Bus1 Development

This contains notes relevant to the out-of-tree bus1 development. This is not
meant to be merged upstream.

## Maintenance Makefile

`./m` is a self-executing makefile to helper development of the bus1 module. If
invoked without further arguments, it will print all valid targets.

## Extensions

The following extensions have been developed in the past, or have been
discussed extensively, and are thus likely to see a comeback:

 - promises
 - oneshot handles
 - FD-passing through FD-nodes
 - unmanaged IDs
 - atomic node-release and queue-flush
 - compound commands
 - file-system pins
 - streaming capabilities
 - forwarding nodes
 - anycast nodes

## Todo

A list of items that need to be resolved at some point:

 - `Message.transfers` should be abstracted and always wrap valid Arcs, but
   store them as raw pointers as needed by `Peer::peek()`.
