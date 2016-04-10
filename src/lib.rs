#[macro_use]
extern crate mopa;
extern crate pulse;
extern crate threadpool;
extern crate fnv;

use std::any::TypeId;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use mopa::Any;
use pulse::{Pulse, Signal};
use threadpool::ThreadPool;

pub use storage::{Storage, StorageBase, VecStorage, HashMapStorage};

mod storage;

/// Index generation. When a new entity is placed at the old index,
/// it bumps the generation by 1. This allows to avoid using components
/// from the entities that were deleted.
/// G<=0 - the entity of generation G is dead
/// G >0 - the entity of generation G is alive
pub type Generation = i32;
/// Index type is arbitrary. It doesn't show up in any interfaces.
/// Keeping it 32bit allows for a single 64bit word per entity.
pub type Index = u32;
/// Entity type, as seen by the user.
#[derive(Clone, Copy, Debug, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct Entity(Index, Generation);

impl Entity {
    #[cfg(test)]
    /// Create a new entity (externally from ECS)
    pub fn new(index: u32, gen: i32) -> Entity {
        Entity(index, gen)
    }

    /// Get the index of the entity.
    pub fn get_id(&self) -> usize { self.0 as usize }
    /// Get the generation of the entity.
    pub fn get_gen(&self) -> Generation { self.1 }
}

/// A custom entity iterator. Needed because the world doesn't really store
/// entities directly, but rather has just a vector of Index -> Generation.
pub struct EntityIter<'a> {
    guard: RwLockReadGuard<'a, Vec<Generation>>,
    index: usize,
}

impl<'a> Iterator for EntityIter<'a> {
    type Item = Entity;
    fn next(&mut self) -> Option<Entity> {
        loop {
            match self.guard.get(self.index) {
                Some(&gen) if gen > 0 => {
                    let ent = Entity(self.index as Index, gen);
                    self.index += 1;
                    return Some(ent)
                },
                Some(_) => self.index += 1, // continue
                None => return None,
            }
        }
    }
}

/// Abstract component type. Doesn't have to be Copy or even Clone.
pub trait Component: Any + Sized {
    type Storage: Storage<Self> + Any + Send + Sync;
}

trait StorageLock: Any + Send + Sync {
    fn del_slice(&self, &[Entity]);
}

mopafy!(StorageLock);

impl<S: StorageBase + Any + Send + Sync> StorageLock for RwLock<S> {
    fn del_slice(&self, entities: &[Entity]) {
        let mut guard = self.write().unwrap();
        for &e in entities.iter() {
            guard.del(e);
        }
    }
}


/// The world struct contains all the data, which is entities and their components.
/// The methods are supposed to be valid for any context they are available in.
pub struct World {
    generations: RwLock<Vec<Generation>>,
    components: HashMap<TypeId, Box<StorageLock>>,
}


impl World {
    /// Create a new empty world.
    pub fn new() -> World {
        World {
            generations: RwLock::new(Vec::new()),
            components: HashMap::new(),
        }
    }
    /// Register a new component type.
    pub fn register<T: Component>(&mut self) {
        let any = RwLock::new(T::Storage::new());
        self.components.insert(TypeId::of::<T>(), Box::new(any));
    }
    /// Unregister a component type.
    pub fn unregister<T: Component>(&mut self) -> Option<T::Storage> {
        self.components.remove(&TypeId::of::<T>()).map(|boxed|
            match boxed.downcast::<RwLock<T::Storage>>() {
                Ok(b) => (*b).into_inner().unwrap(),
                Err(_) => panic!("Unable to downcast the storage type"),
            }
        )
    }
    fn lock<T: Component>(&self) -> &RwLock<T::Storage> {
        let boxed = self.components.get(&TypeId::of::<T>()).unwrap();
        boxed.downcast_ref().unwrap()
    }
    /// Lock a component for reading.
    pub fn read<'a, T: Component>(&'a self) -> RwLockReadGuard<'a, T::Storage> {
        self.lock::<T>().read().unwrap()
    }
    /// Lock a component for writing.
    pub fn write<'a, T: Component>(&'a self) -> RwLockWriteGuard<'a, T::Storage> {
        self.lock::<T>().write().unwrap()
    }
    /// Return the entity iterator.
    pub fn entities<'a>(&'a self) -> EntityIter<'a> {
        EntityIter {
            guard: self.generations.read().unwrap(),
            index: 0,
        }
    }
    fn find_next(&self, base: usize) -> Entity {
        let gens = self.generations.read().unwrap();
        match gens.iter().enumerate().skip(base).find(|&(_, g)| *g <= 0) {
            Some((id, gen)) => Entity(id as Index, 1 - gen),
            None => Entity(gens.len() as Index, 1),
        }
    }
    /// Return the generations array locked for reading. Useful for debugging.
    pub fn get_generations<'a>(&'a self) -> RwLockReadGuard<'a, Vec<Generation>> {
        self.generations.read().unwrap()
    }
}


struct Appendix {
    next: Entity,
    add_queue: Vec<Entity>,
    sub_queue: Vec<Entity>
}

/// A custom entity iterator for dynamically added entities.
pub struct NewEntityIter<'a> {
    guard: RwLockReadGuard<'a, Appendix>,
    index: usize,
}

impl<'a> Iterator for NewEntityIter<'a> {
    type Item = Entity;
    fn next(&mut self) -> Option<Entity> {
        let ent = self.guard.add_queue.get(self.index);
        self.index += 1;
        ent.map(|e| *e)
    }
}

/// World argument for a system closure.
pub struct WorldArg {
    world: Arc<World>,
    pulse: RefCell<Option<Pulse>>,
    app: Arc<RwLock<Appendix>>,
}

impl WorldArg {
    /// Borrows the world, allowing the system lock some components and get the entity
    /// iterator. Has to be called only once. Fires a pulse at the end.
    pub fn fetch<'a, U, F>(&'a self, f: F) -> U
        where F: FnOnce(&'a World) -> U
    {
        let pulse = self.pulse.borrow_mut().take()
                        .expect("fetch may only be called once.");
        let u = f(&self.world);
        pulse.pulse();
        u
    }
    /// Insert a new entity dynamically.
    pub fn insert(&self) -> Entity {
        let mut app = self.app.write().unwrap();
        let ent = app.next;
        app.add_queue.push(ent);
        app.next = self.world.find_next(ent.get_id() + 1);
        ent
    }
    /// Remove an entity dynamically.
    pub fn remove(&self, entity: Entity) {
        let mut app = self.app.write().unwrap();
        app.sub_queue.push(entity);
    }
    /// Iterate dynamically added entities.
    pub fn new_entities<'a>(&'a self) -> NewEntityIter<'a> {
        NewEntityIter {
            guard: self.app.read().unwrap(),
            index: 0,
        }
    }
}

/// Helper builder for entities.
pub struct EntityBuilder<'a>(Entity, &'a World);

impl<'a> EntityBuilder<'a> {
    /// Add a component value to the new entity.
    pub fn with<T: Component>(self, value: T) -> EntityBuilder<'a> {
        self.1.write::<T>().insert(self.0, value);
        self
    }
    /// Finish entity construction.
    pub fn build(self) -> Entity {
        self.0
    }
}


pub struct Scheduler {
    world: Arc<World>,
    threads: ThreadPool,
    appendix: Arc<RwLock<Appendix>>,
}

impl Scheduler {
    pub fn new(world: World, num_threads: usize) -> Scheduler {
        let next = Appendix {
            next: world.find_next(0),
            add_queue: Vec::new(),
            sub_queue: Vec::new(),
        };
        Scheduler {
            world: Arc::new(world),
            threads: ThreadPool::new(num_threads),
            appendix: Arc::new(RwLock::new(next)),
        }
    }
    pub fn get_world(&self) -> &World {
        &self.world
    }
    pub fn run<F>(&mut self, functor: F) where
        F: 'static + Send + FnOnce(WorldArg)
    {
        let (signal, pulse) = Signal::new();
        let world = self.world.clone();
        let app = self.appendix.clone();
        self.threads.execute(|| {
            functor(WorldArg {
                world: world,
                pulse: RefCell::new(Some(pulse)),
                app: app,
            });
        });
        signal.wait().unwrap();
    }
    pub fn add_entity<'a>(&'a mut self) -> EntityBuilder<'a> {
        let mut appendix = self.appendix.write().unwrap();
        let ent = appendix.next;
        assert!(ent.get_gen() > 0);
        if ent.get_gen() == 1 {
            let mut gens = self.world.generations.write().unwrap();
            assert!(gens.len() == ent.get_id());
            gens.push(ent.get_gen());
        }
        appendix.next = self.world.find_next(ent.get_id() + 1);
        EntityBuilder(ent, &self.world)
    }
    pub fn del_entity(&mut self, entity: Entity) {
        for boxed in self.world.components.values() {
            boxed.del_slice(&[entity]);
        }
        let mut gens = self.world.generations.write().unwrap();
        let mut gen = &mut gens[entity.get_id() as usize];
        assert!(*gen > 0);
        let mut appendix = self.appendix.write().unwrap();
        if entity.get_id() < appendix.next.get_id() {
            appendix.next = Entity(entity.0, *gen+1);
        }
        *gen *= -1;
    }
    pub fn rest(&self) {
        let mut gens = self.world.generations.write().unwrap();
        let mut app = self.appendix.write().unwrap();
        for ent in app.add_queue.drain(..) {
            while gens.len() <= ent.get_id() {
                gens.push(0);
            }
            assert_eq!(ent.get_gen(), 1 - gens[ent.get_id()]);
            gens[ent.get_id()] = ent.get_gen();
        }
        let mut next = app.next;
        for ent in app.sub_queue.drain(..) {
            assert_eq!(ent.get_gen(), gens[ent.get_id()]);
            if ent.get_id() < next.get_id() {
                next = Entity(ent.0, ent.1 + 1);
            }
            gens[ent.get_id()] *= -1;
        }
        app.next = next;
    }
}

macro_rules! impl_run {
    ($name:ident [$( $write:ident ),*] [$( $read:ident ),*]) => (impl Scheduler {
        #[allow(non_snake_case, unused_mut)]
        pub fn $name<
            $($write:Component,)* $($read:Component,)*
            F: 'static + Send + FnMut( $(&mut $write,)* $(&$read,)* )
        >(&mut self, functor: F) {
            self.run(|warg| {
                let mut fun = functor;
                let ($(mut $write,)* $($read,)* entities) = warg.fetch(|w|
                    ($(w.write::<$write>(),)*
                     $(w.read::<$read>(),)*
                       w.entities())
                );
                for ent in entities {
                    if let ( $( Some($write), )* $( Some($read), )* ) =
                        ( $( $write.get_mut(ent), )* $( $read.get(ent), )* ) {
                        fun( $($write,)* $($read,)* );
                    }
                }
                for ent in warg.new_entities() {
                    if let ( $( Some($write), )* $( Some($read), )* ) =
                        ( $( $write.get_mut(ent), )* $( $read.get(ent), )* ) {
                        fun( $($write,)* $($read,)* );
                    }
                }
            });
        }
    })
}

impl_run!( run0w1r [] [R0] );
impl_run!( run0w2r [] [R0, R1] );
impl_run!( run1w0r [W0] [] );
impl_run!( run1w1r [W0] [R0] );
impl_run!( run1w2r [W0] [R0, R1] );
impl_run!( run1w3r [W0] [R0, R1, R2] );
impl_run!( run1w4r [W0] [R0, R1, R2, R3] );
impl_run!( run1w5r [W0] [R0, R1, R2, R3, R4] );
impl_run!( run1w6r [W0] [R0, R1, R2, R3, R4, R5] );
impl_run!( run1w7r [W0] [R0, R1, R2, R3, R5, R6, R7] );
impl_run!( run2w0r [W0, W1] [] );
impl_run!( run2w1r [W0, W1] [R0] );
impl_run!( run2w2r [W0, W1] [R0, R1] );