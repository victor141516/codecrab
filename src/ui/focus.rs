use std::{collections::HashMap, hash::Hash};

use ratatui::layout::{Position, Rect};

#[derive(Clone, Debug)]
struct InteractiveControl<Id, Action, Scope> {
    id: Id,
    area: Rect,
    action: Option<Action>,
    enabled: bool,
    scope: Scope,
}

pub(super) struct InteractionRegistry<Id, Action, Scope> {
    previous: Vec<InteractiveControl<Id, Action, Scope>>,
    controls: Vec<InteractiveControl<Id, Action, Scope>>,
    focused: HashMap<Scope, Id>,
    defaults: HashMap<Scope, Id>,
    active_scope: Scope,
}

impl<Id, Action, Scope> InteractionRegistry<Id, Action, Scope>
where
    Id: Clone + Eq,
    Action: Clone,
    Scope: Clone + Eq + Hash,
{
    pub(super) fn new(default_scope: Scope, default_focus: Id) -> Self {
        Self {
            previous: Vec::new(),
            controls: Vec::new(),
            focused: HashMap::from([(default_scope.clone(), default_focus.clone())]),
            defaults: HashMap::from([(default_scope.clone(), default_focus)]),
            active_scope: default_scope,
        }
    }

    pub(super) fn begin_frame(&mut self, active_scope: Scope) {
        self.previous = std::mem::take(&mut self.controls);
        self.active_scope = active_scope;
    }

    pub(super) fn register(
        &mut self,
        id: Id,
        area: Rect,
        action: Option<Action>,
        enabled: bool,
        scope: Scope,
    ) {
        self.controls.push(InteractiveControl {
            id,
            area,
            action,
            enabled,
            scope,
        });
    }

    pub(super) fn finish_frame(&mut self) {
        let scopes = self.focused.keys().cloned().collect::<Vec<_>>();
        for scope in scopes {
            let Some(focused) = self.focused.get(&scope).cloned() else {
                continue;
            };
            if self
                .controls
                .iter()
                .any(|control| control.enabled && control.scope == scope && control.id == focused)
            {
                continue;
            }

            let current = self
                .controls
                .iter()
                .filter(|control| control.enabled && control.scope == scope)
                .map(|control| control.id.clone())
                .collect::<Vec<_>>();
            if current.is_empty() {
                self.focused.remove(&scope);
                continue;
            }

            let previous = self
                .previous
                .iter()
                .filter(|control| control.enabled && control.scope == scope)
                .map(|control| control.id.clone())
                .collect::<Vec<_>>();
            let replacement = previous
                .iter()
                .position(|id| id == &focused)
                .and_then(|index| {
                    previous[index + 1..]
                        .iter()
                        .find(|id| current.contains(id))
                        .or_else(|| {
                            previous[..index]
                                .iter()
                                .rev()
                                .find(|id| current.contains(id))
                        })
                        .cloned()
                })
                .or_else(|| {
                    self.defaults
                        .get(&scope)
                        .filter(|id| current.contains(id))
                        .cloned()
                })
                .unwrap_or_else(|| current[0].clone());
            self.focused.insert(scope, replacement);
        }

        for (scope, default) in &self.defaults {
            if !self.focused.contains_key(scope)
                && self.controls.iter().any(|control| {
                    control.enabled && control.scope == *scope && control.id == *default
                })
            {
                self.focused.insert(scope.clone(), default.clone());
            }
        }
    }

    pub(super) fn is_focused(&self, scope: &Scope, id: &Id) -> bool {
        self.active_scope == *scope && self.focused.get(scope) == Some(id)
    }

    pub(super) fn focus(&mut self, scope: &Scope, id: &Id) -> bool {
        if !self
            .controls
            .iter()
            .any(|control| control.enabled && &control.scope == scope && &control.id == id)
        {
            return false;
        }
        self.focused.insert(scope.clone(), id.clone());
        true
    }

    pub(super) fn focus_active(&mut self, id: &Id) -> bool {
        let scope = self.active_scope.clone();
        self.focus(&scope, id)
    }

    pub(super) fn move_focus(&mut self, forward: bool) -> Option<Id> {
        let ids = self
            .controls
            .iter()
            .filter(|control| control.enabled && control.scope == self.active_scope)
            .map(|control| control.id.clone())
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return None;
        }
        let current = self.focused.get(&self.active_scope);
        let next = current
            .and_then(|focused| ids.iter().position(|id| id == focused))
            .map(|index| {
                if forward {
                    (index + 1) % ids.len()
                } else {
                    (index + ids.len() - 1) % ids.len()
                }
            })
            .unwrap_or_else(|| if forward { 0 } else { ids.len() - 1 });
        let id = ids[next].clone();
        self.focused.insert(self.active_scope.clone(), id.clone());
        Some(id)
    }

    pub(super) fn focused_id(&self) -> Option<&Id> {
        self.focused.get(&self.active_scope)
    }

    pub(super) fn focused_action(&self) -> Option<Action> {
        let focused = self.focused_id()?;
        self.controls
            .iter()
            .find(|control| {
                control.enabled && control.scope == self.active_scope && &control.id == focused
            })
            .and_then(|control| control.action.clone())
    }

    pub(super) fn hit_test(&self, position: Position) -> Option<(Id, Option<Action>)> {
        self.controls
            .iter()
            .rev()
            .find(|control| {
                control.enabled
                    && control.scope == self.active_scope
                    && control.area.contains(position)
            })
            .map(|control| (control.id.clone(), control.action.clone()))
    }

    #[cfg(test)]
    pub(super) fn area(&self, id: &Id) -> Option<Rect> {
        self.controls
            .iter()
            .find(|control| &control.id == id)
            .map(|control| control.area)
    }

    #[cfg(test)]
    pub(super) fn focusable_count(&self, scope: &Scope) -> usize {
        self.controls
            .iter()
            .filter(|control| control.enabled && &control.scope == scope)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    enum Scope {
        Root,
        Dialog,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Id {
        Composer,
        One,
        Two,
        Three,
        Dialog,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Action {
        Activate,
    }

    fn register(
        registry: &mut InteractionRegistry<Id, Action, Scope>,
        id: Id,
        enabled: bool,
        scope: Scope,
    ) {
        registry.register(
            id,
            Rect::new(0, 0, 1, 1),
            (id != Id::Composer).then_some(Action::Activate),
            enabled,
            scope,
        );
    }

    #[test]
    fn traversal_wraps_forward_and_backward_and_skips_disabled_controls() {
        let mut registry = InteractionRegistry::new(Scope::Root, Id::Composer);
        registry.begin_frame(Scope::Root);
        register(&mut registry, Id::One, true, Scope::Root);
        register(&mut registry, Id::Two, false, Scope::Root);
        register(&mut registry, Id::Three, true, Scope::Root);
        register(&mut registry, Id::Composer, true, Scope::Root);
        registry.finish_frame();

        assert_eq!(registry.move_focus(true), Some(Id::One));
        assert_eq!(registry.move_focus(true), Some(Id::Three));
        assert_eq!(registry.move_focus(true), Some(Id::Composer));
        assert_eq!(registry.move_focus(false), Some(Id::Three));
    }

    #[test]
    fn focus_survives_rerenders_and_moves_to_the_nearest_control_after_removal() {
        let mut registry = InteractionRegistry::new(Scope::Root, Id::Composer);
        registry.begin_frame(Scope::Root);
        register(&mut registry, Id::One, true, Scope::Root);
        register(&mut registry, Id::Two, true, Scope::Root);
        register(&mut registry, Id::Three, true, Scope::Root);
        register(&mut registry, Id::Composer, true, Scope::Root);
        registry.finish_frame();
        assert!(registry.focus(&Scope::Root, &Id::Two));

        registry.begin_frame(Scope::Root);
        register(&mut registry, Id::One, true, Scope::Root);
        register(&mut registry, Id::Three, true, Scope::Root);
        register(&mut registry, Id::Composer, true, Scope::Root);
        registry.finish_frame();

        assert_eq!(registry.focused_id(), Some(&Id::Three));
    }

    #[test]
    fn active_scope_blocks_background_traversal_and_hit_testing() {
        let mut registry = InteractionRegistry::new(Scope::Root, Id::Composer);
        registry.begin_frame(Scope::Dialog);
        register(&mut registry, Id::One, true, Scope::Root);
        register(&mut registry, Id::Composer, true, Scope::Root);
        register(&mut registry, Id::Dialog, true, Scope::Dialog);
        registry.finish_frame();

        assert_eq!(registry.move_focus(true), Some(Id::Dialog));
        assert_eq!(
            registry.hit_test(Position::new(0, 0)),
            Some((Id::Dialog, Some(Action::Activate)))
        );

        registry.begin_frame(Scope::Root);
        register(&mut registry, Id::One, true, Scope::Root);
        register(&mut registry, Id::Composer, true, Scope::Root);
        registry.finish_frame();
        assert_eq!(registry.focused_id(), Some(&Id::Composer));
    }
}
