use super::{
    command::CommandBuffer,
    resources::{Resource, ResourceSet, ResourceTypeId, Resources},
    schedule::Runnable,
};
use crate::{
    cons::{ConsAppend, ConsFlatten},
    permissions::Permissions,
    query::{
        filter::EntityFilter,
        view::{read::Read, write::Write, View},
        Query,
    },
    storage::{
        archetype::ArchetypeIndex,
        component::{Component, ComponentTypeId},
    },
    subworld::{ArchetypeAccess, ComponentAccess, SubWorld},
    world::{World, WorldId},
};
use bit_set::BitSet;
use std::{any::TypeId, borrow::Cow, collections::HashMap, marker::PhantomData};
use tracing::{debug, info, span, Level};

/// Provides an abstraction across tuples of queries for system closures.
pub trait QuerySet: Send + Sync {
    /// Evaluates the queries and records which archetypes they require access to into a bitset.
    fn filter_archetypes(&mut self, world: &World, archetypes: &mut BitSet);
}

macro_rules! queryset_tuple {
    ($head_ty:ident) => {
        impl_queryset_tuple!($head_ty);
    };
    ($head_ty:ident, $( $tail_ty:ident ),*) => (
        impl_queryset_tuple!($head_ty, $( $tail_ty ),*);
        queryset_tuple!($( $tail_ty ),*);
    );
}

macro_rules! impl_queryset_tuple {
    ($($ty: ident),*) => {
            #[allow(unused_parens, non_snake_case)]
            impl<$( $ty, )*> QuerySet for ($( $ty, )*)
            where
                $( $ty: QuerySet, )*
            {
                fn filter_archetypes(&mut self, world: &World, bitset: &mut BitSet) {
                    let ($($ty,)*) = self;

                    $( $ty.filter_archetypes(world, bitset); )*
                }
            }
    };
}

#[cfg(feature = "extended-tuple-impls")]
queryset_tuple!(A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z);

#[cfg(not(feature = "extended-tuple-impls"))]
queryset_tuple!(A, B, C, D, E, F, G, H);

impl QuerySet for () {
    fn filter_archetypes(&mut self, _: &World, _: &mut BitSet) {}
}

impl<AV, AF> QuerySet for Query<AV, AF>
where
    AV: for<'v> View<'v> + Send + Sync,
    AF: EntityFilter,
{
    fn filter_archetypes(&mut self, world: &World, bitset: &mut BitSet) {
        for &ArchetypeIndex(arch) in self.find_archetypes(world) {
            bitset.insert(arch as usize);
        }
    }
}

/// Structure describing the resource and component access conditions of the system.
#[derive(Debug, Clone)]
pub struct SystemAccess {
    resources: Permissions<ResourceTypeId>,
    components: Permissions<ComponentTypeId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SystemId {
    name: Cow<'static, str>,
    type_id: TypeId,
}

struct Unspecified;

impl SystemId {
    pub fn of<T: 'static>(name: Option<String>) -> Self {
        Self {
            name: name
                .unwrap_or_else(|| std::any::type_name::<T>().to_string())
                .into(),
            type_id: TypeId::of::<T>(),
        }
    }
}

impl std::fmt::Display for SystemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl<T: Into<Cow<'static, str>>> From<T> for SystemId {
    fn from(name: T) -> SystemId {
        SystemId {
            name: name.into(),
            type_id: TypeId::of::<Unspecified>(),
        }
    }
}

/// The concrete type which contains the system closure provided by the user.  This struct should
/// not be instantiated directly, and instead should be created using `SystemBuilder`.
///
/// Implements `Schedulable` which is consumable by the `StageExecutor`, executing the closure.
///
/// Also handles caching of archetype information in a `BitSet`, as well as maintaining the provided
/// information about what queries this system will run and, as a result, its data access.
///
/// Queries are stored generically within this struct, and the `SystemQuery` types are generated
/// on each `run` call, wrapping the world and providing the set to the user in their closure.
pub struct System<R, Q, F> {
    name: SystemId,
    _resources: PhantomData<R>,
    queries: Q,
    run_fn: F,
    archetypes: ArchetypeAccess,
    access: SystemAccess,

    // We pre-allocate a command buffer for ourself. Writes are self-draining so we never have to rellocate.
    command_buffer: HashMap<WorldId, CommandBuffer>,
}

impl<R, Q, F> Runnable for System<R, Q, F>
where
    R: for<'a> ResourceSet<'a>,
    Q: QuerySet,
    F: SystemFn<R, Q>,
{
    fn name(&self) -> &SystemId { &self.name }

    fn reads(&self) -> (&[ResourceTypeId], &[ComponentTypeId]) {
        (
            &self.access.resources.reads(),
            &self.access.components.reads(),
        )
    }

    fn writes(&self) -> (&[ResourceTypeId], &[ComponentTypeId]) {
        (
            &self.access.resources.writes(),
            &self.access.components.writes(),
        )
    }

    fn prepare(&mut self, world: &World) {
        if let ArchetypeAccess::Some(bitset) = &mut self.archetypes {
            self.queries.filter_archetypes(world, bitset);
        }
    }

    fn accesses_archetypes(&self) -> &ArchetypeAccess { &self.archetypes }

    fn command_buffer_mut(&mut self, world: WorldId) -> Option<&mut CommandBuffer> {
        self.command_buffer.get_mut(&world)
    }

    unsafe fn run_unsafe(&mut self, world: &World, resources: &Resources) {
        let span = span!(Level::INFO, "System", system = %self.name);
        let _guard = span.enter();

        debug!("Initializing");

        // safety:
        // It is difficult to correctly communicate the lifetime of the resource fetch through to the system closure.
        // We are hacking this by passing the fetch with a static lifetime to its internal references.
        // This is sound because the fetch structs only provide access to the resource through reborrows on &self.
        // As the fetch struct is created on the stack here, and the resources it is holding onto is a parameter to this function,
        // we know for certain that the lifetime of the fetch struct (which constrains the lifetime of the resource the system sees)
        // must be shorter than the lifetime of the resource.
        let resources_static = std::mem::transmute::<_, &'static Resources>(resources);
        let mut resources = R::fetch_unchecked(resources_static);

        let queries = &mut self.queries;
        let component_access = ComponentAccess::Allow(Cow::Borrowed(&self.access.components));
        let mut world_shim =
            SubWorld::new_unchecked(world, component_access, self.archetypes.bitset());
        let cmd = self
            .command_buffer
            .entry(world.id())
            .or_insert_with(|| CommandBuffer::new(world));

        info!("Running");
        let borrow = &mut self.run_fn;
        borrow.run(cmd, &mut world_shim, &mut resources, queries);
    }
}

/// Supertrait used for defining systems. All wrapper objects for systems implement this trait.
///
/// This trait will generally not be used by users.
pub trait SystemFn<R: ResourceSet<'static>, Q: QuerySet> {
    // type Resources: ResourceSet<'static>;
    // type Queries;

    fn run(
        &mut self,
        commands: &mut CommandBuffer,
        world: &mut SubWorld,
        resources: &mut R::Result,
        queries: &mut Q,
    );
}

impl<F, R, Q> SystemFn<R, Q> for F
where
    R: ResourceSet<'static>,
    Q: QuerySet,
    F: FnMut(&mut CommandBuffer, &mut SubWorld, &mut R::Result, &mut Q) + 'static,
{
    // type Resources = R;
    // type Queries = Q;

    fn run(
        &mut self,
        commands: &mut CommandBuffer,
        world: &mut SubWorld,
        resources: &mut R::Result,
        queries: &mut Q,
    ) {
        (self)(commands, world, resources, queries)
    }
}

// pub struct SystemFnWrapper<
//     R: ResourceSet<'static>,
//     Q,
//     F: FnMut(&mut CommandBuffer, &mut SubWorld, &mut R::Result, &mut Q) + 'static,
// >(F, PhantomData<(R, Q)>);

// impl<'a, F, R: ResourceSet<'static>, Q> SystemFn for SystemFnWrapper<R, Q, F>
// where
//     F: FnMut(&mut CommandBuffer, &mut SubWorld, &mut R, &mut Q) + 'static,
// {
//     type Resources = R;
//     type Queries = Q;

//     fn run(
//         &mut self,
//         commands: &mut CommandBuffer,
//         world: &mut SubWorld,
//         resources: &mut Self::Resources,
//         queries: &mut Self::Queries,
//     ) {
//         (self.0)(commands, world, resources, queries);
//     }
// }

// This builder uses a Cons/Hlist implemented in cons.rs to generated the static query types
// for this system. Access types are instead stored and abstracted in the top level vec here
// so the underlying ResourceSet type functions from the queries don't need to allocate.
// Otherwise, this leads to excessive alloaction for every call to reads/writes
/// The core builder of `System` types, which are systems within Legion. Systems are implemented
/// as singular closures for a given system - providing queries which should be cached for that
/// system, as well as resource access and other metadata.
/// ```rust
/// # use legion::query::view::{read::Read, write::Write};
/// # use legion::query::filter::filter_fns::*;
/// # use legion::systems::system::*;
/// # use legion::entity::Entity;
/// # use legion::query::IntoQuery;
/// # #[derive(Copy, Clone, Debug, PartialEq)]
/// # struct Position;
/// # #[derive(Copy, Clone, Debug, PartialEq)]
/// # struct Velocity;
/// # #[derive(Copy, Clone, Debug, PartialEq)]
/// # struct Model;
/// #[derive(Copy, Clone, Debug, PartialEq)]
/// struct Static;
/// #[derive(Debug)]
/// struct TestResource {}
///
///  let mut system_one = SystemBuilder::new("TestSystem")
///            .read_resource::<TestResource>()
///            .with_query(<(Entity, Read<Position>, Read<Model>)>::query()
///                         .filter(!component::<Static>() | maybe_changed::<Position>()))
///            .build(move |commands, world, resource, queries| {
///                for (entity, pos, model) in queries.iter_mut(world) {
///
///                }
///            });
/// ```
pub struct SystemBuilder<Q = (), R = ()> {
    name: SystemId,
    queries: Q,
    resources: R,
    resource_access: Permissions<ResourceTypeId>,
    component_access: Permissions<ComponentTypeId>,
    access_all_archetypes: bool,
}

impl SystemBuilder<(), ()> {
    /// Create a new system builder to construct a new system.
    ///
    /// Please note, the `name` argument for this method is just for debugging and visualization
    /// purposes and is not logically used anywhere.
    pub fn new<T: Into<SystemId>>(name: T) -> Self {
        Self {
            name: name.into(),
            queries: (),
            resources: (),
            resource_access: Permissions::default(),
            component_access: Permissions::default(),
            access_all_archetypes: false,
        }
    }
}

impl<Q, R> SystemBuilder<Q, R>
where
    Q: 'static + Send + ConsFlatten,
    R: 'static + Send + ConsFlatten,
{
    /// Defines a query to provide this system for its execution. Multiple queries can be provided,
    /// and queries are cached internally for efficiency for filtering and archetype ID handling.
    ///
    /// It is best practice to define your queries here, to allow for the caching to take place.
    /// These queries are then provided to the executing closure as a tuple of queries.
    pub fn with_query<V, F>(
        mut self,
        query: Query<V, F>,
    ) -> SystemBuilder<<Q as ConsAppend<Query<V, F>>>::Output, R>
    where
        V: for<'a> View<'a>,
        F: 'static + EntityFilter,
        Q: ConsAppend<Query<V, F>>,
    {
        self.component_access.add(V::requires_permissions());

        SystemBuilder {
            name: self.name,
            queries: ConsAppend::append(self.queries, query),
            resources: self.resources,
            resource_access: self.resource_access,
            component_access: self.component_access,
            access_all_archetypes: self.access_all_archetypes,
        }
    }

    /// Flag this resource type as being read by this system.
    ///
    /// This will inform the dispatcher to not allow any writes access to this resource while
    /// this system is running. Parralel reads still occur during execution.
    pub fn read_resource<T>(mut self) -> SystemBuilder<Q, <R as ConsAppend<Read<T>>>::Output>
    where
        T: 'static + Resource,
        R: ConsAppend<Read<T>>,
        <R as ConsAppend<Read<T>>>::Output: ConsFlatten,
    {
        self.resource_access.push_read(ResourceTypeId::of::<T>());

        SystemBuilder {
            name: self.name,
            queries: self.queries,
            resources: ConsAppend::append(self.resources, Read::<T>::default()),
            resource_access: self.resource_access,
            component_access: self.component_access,
            access_all_archetypes: self.access_all_archetypes,
        }
    }

    /// Flag this resource type as being written by this system.
    ///
    /// This will inform the dispatcher to not allow any parallel access to this resource while
    /// this system is running.
    pub fn write_resource<T>(mut self) -> SystemBuilder<Q, <R as ConsAppend<Write<T>>>::Output>
    where
        T: 'static + Resource,
        R: ConsAppend<Write<T>>,
        <R as ConsAppend<Write<T>>>::Output: ConsFlatten,
    {
        self.resource_access.push(ResourceTypeId::of::<T>());

        SystemBuilder {
            name: self.name,
            queries: self.queries,
            resources: ConsAppend::append(self.resources, Write::<T>::default()),
            resource_access: self.resource_access,
            component_access: self.component_access,
            access_all_archetypes: self.access_all_archetypes,
        }
    }

    /// This performs a soft resource block on the component for writing. The dispatcher will
    /// generally handle dispatching read and writes on components based on archetype, allowing
    /// for more granular access and more parallelization of systems.
    ///
    /// Using this method will mark the entire component as read by this system, blocking writing
    /// systems from accessing any archetypes which contain this component for the duration of its
    /// execution.
    ///
    /// This type of access with `SubWorld` is provided for cases where sparse component access
    /// is required and searching entire query spaces for entities is inefficient.
    pub fn read_component<T>(mut self) -> Self
    where
        T: Component,
    {
        self.component_access.push_read(ComponentTypeId::of::<T>());
        self.access_all_archetypes = true;

        self
    }

    /// This performs a exclusive resource block on the component for writing. The dispatcher will
    /// generally handle dispatching read and writes on components based on archetype, allowing
    /// for more granular access and more parallelization of systems.
    ///
    /// Using this method will mark the entire component as written by this system, blocking other
    /// systems from accessing any archetypes which contain this component for the duration of its
    /// execution.
    ///
    /// This type of access with `SubWorld` is provided for cases where sparse component access
    /// is required and searching entire query spaces for entities is inefficient.
    pub fn write_component<T>(mut self) -> Self
    where
        T: Component,
    {
        self.component_access.push(ComponentTypeId::of::<T>());
        self.access_all_archetypes = true;

        self
    }

    // /// Builds a standard legion `System`. A system is considered a closure for all purposes. This
    // /// closure is `FnMut`, allowing for capture of variables for tracking state for this system.
    // /// Instead of the classic OOP architecture of a system, this lets you still maintain state
    // /// across execution of the systems while leveraging the type semantics of closures for better
    // /// ergonomics.
    // pub fn build<F>(self, run_fn: F) -> Box<dyn Schedulable>
    // where
    //     <R as ConsFlatten>::Output: for<'a> ResourceSet<'a> + Send + Sync,
    //     <Q as ConsFlatten>::Output: QuerySet + Send + Sync,
    //     <<R as ConsFlatten>::Output as ResourceSet>::Result: Send + Sync,
    //     F: FnMut(
    //             &mut CommandBuffer,
    //             &mut SubWorld,
    //             &mut <<R as ConsFlatten>::Output as ResourceSet>::Result,
    //             &mut <Q as ConsFlatten>::Output,
    //         ) + Send
    //         + Sync
    //         + 'static,
    // {
    //     let run_fn = SystemFnWrapper(run_fn, PhantomData);
    //     Box::new(System {
    //         name: self.name,
    //         run_fn: run_fn,
    //         _resources: PhantomData::<<R as ConsFlatten>::Output>,
    //         queries: self.queries.flatten(),
    //         archetypes: if self.access_all_archetypes {
    //             ArchetypeAccess::All
    //         } else {
    //             ArchetypeAccess::Some(BitSet::default())
    //         },
    //         access: SystemAccess {
    //             resources: self.resource_access,
    //             components: self.component_access,
    //         },
    //         command_buffer: HashMap::default(),
    //     })
    // }

    /// Builds a system which is not `Schedulable`, as it is not thread safe (!Send and !Sync),
    /// but still implements all the calling infrastructure of the `Runnable` trait. This provides
    /// a way for legion consumers to leverage the `System` construction and type-handling of
    /// this build for thread local systems which cannot leave the main initializing thread.
    pub fn build<F>(
        self,
        run_fn: F,
    ) -> System<<R as ConsFlatten>::Output, <Q as ConsFlatten>::Output, F>
    where
        <R as ConsFlatten>::Output: for<'a> ResourceSet<'a> + Send + Sync,
        <Q as ConsFlatten>::Output: QuerySet,
        F: FnMut(
            &mut CommandBuffer,
            &mut SubWorld,
            &mut <<R as ConsFlatten>::Output as ResourceSet<'static>>::Result,
            &mut <Q as ConsFlatten>::Output,
        ),
    {
        System {
            name: self.name,
            run_fn: run_fn,
            _resources: PhantomData::<<R as ConsFlatten>::Output>,
            queries: self.queries.flatten(),
            archetypes: if self.access_all_archetypes {
                ArchetypeAccess::All
            } else {
                ArchetypeAccess::Some(BitSet::default())
            },
            access: SystemAccess {
                resources: self.resource_access,
                components: self.component_access,
            },
            command_buffer: HashMap::default(),
        }
    }
}