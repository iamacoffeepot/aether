//! Type-level list of reactors sharing one [`Owner`].

use alloc::format;
use alloc::vec::Vec;

use aether_bloomery_kinds::{Detail, ReactorIntent, ReactorName, RuleName};

use crate::error::PrepareError;
use crate::evaluate::{ArmVisitor, Intent, Output, Reactor};
use crate::owner::Owner;
use crate::params::{Nil, Params};
use crate::trigger::Trigger;

mod sealed {
    pub trait Sealed {}
}

/// First reactor that failed while evaluating a live event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluateFail {
    /// Reactor whose evaluation failed.
    pub reactor: ReactorName,
    /// Why it failed.
    pub reason: Detail,
}

/// Ordered reactors folded and evaluated against one shared owner.
///
/// Implemented for [`Nil`] and `(R, Rest)`. The list is type-level: there is
/// no runtime recursion.
pub trait ReactorList: sealed::Sealed + Sized + 'static {
    /// Convert every reactor and rule name once.
    ///
    /// # Errors
    ///
    /// [`Detail`] when a [`Reactor::NAMESPACE`] or rule ident is not a valid
    /// dotted name.
    fn check_names() -> Result<(), Detail>;

    /// Fold every reactor's views to the owner's cursor.
    ///
    /// # Errors
    ///
    /// [`PrepareError`] from a view fold.
    fn warm_all(owner: &mut Owner) -> Result<(), PrepareError>;

    /// Evaluate each reactor in list order. Stop at the first error.
    ///
    /// # Errors
    ///
    /// [`EvaluateFail`] naming the reactor that failed.
    fn evaluate_all(owner: &mut Owner) -> Result<Vec<ReactorIntent>, EvaluateFail>;
}

impl sealed::Sealed for Nil {}

impl ReactorList for Nil {
    fn check_names() -> Result<(), Detail> {
        Ok(())
    }

    fn warm_all(_owner: &mut Owner) -> Result<(), PrepareError> {
        Ok(())
    }

    fn evaluate_all(_owner: &mut Owner) -> Result<Vec<ReactorIntent>, EvaluateFail> {
        Ok(Vec::new())
    }
}

impl<R: Reactor + Default, Rest: ReactorList> sealed::Sealed for (R, Rest) {}

impl<R: Reactor + Default, Rest: ReactorList> ReactorList for (R, Rest) {
    fn check_names() -> Result<(), Detail> {
        reactor_name::<R>()?;
        let mut visitor = NameCheck { error: None };
        R::visit_arms(&mut visitor);
        if let Some(error) = visitor.error {
            return Err(error);
        }
        Rest::check_names()
    }

    fn warm_all(owner: &mut Owner) -> Result<(), PrepareError> {
        warm_reactor::<R>(owner)?;
        Rest::warm_all(owner)
    }

    fn evaluate_all(owner: &mut Owner) -> Result<Vec<ReactorIntent>, EvaluateFail> {
        let reactor = reactor_name::<R>().expect("Root::new accepted this NAMESPACE");
        let mut intents = match R::default().evaluate(owner) {
            Ok(intents) => match tag(&reactor, intents) {
                Ok(intents) => intents,
                Err(reason) => return Err(EvaluateFail { reactor, reason }),
            },
            Err(reason) => {
                return Err(EvaluateFail { reactor, reason: Detail::new(format!("{reason}")) });
            }
        };
        match Rest::evaluate_all(owner) {
            Ok(rest) => {
                intents.extend(rest);
                Ok(intents)
            }
            Err(error) => Err(error),
        }
    }
}

fn reactor_name<R: Reactor>() -> Result<ReactorName, Detail> {
    ReactorName::new(R::NAMESPACE).map_err(|error| Detail::new(format!("{error}")))
}

fn tag(reactor: &ReactorName, intents: Vec<Intent>) -> Result<Vec<ReactorIntent>, Detail> {
    let mut out = Vec::with_capacity(intents.len());
    for intent in intents {
        let (rule, kind, bytes) = intent.into_parts();
        let rule = RuleName::new(rule).map_err(|error| Detail::new(format!("{error}")))?;
        out.push(ReactorIntent::new(reactor.clone(), rule, kind, bytes));
    }
    Ok(out)
}

fn warm_reactor<R: Reactor>(owner: &mut Owner) -> Result<(), PrepareError> {
    struct Warm<'a> {
        owner: &'a mut Owner,
        error: Result<(), PrepareError>,
    }

    impl ArmVisitor for Warm<'_> {
        fn visit<T, L, O>(&mut self, _name: &'static str)
        where
            T: Trigger,
            L: Params<T>,
            O: Output,
        {
            if self.error.is_ok() {
                self.error = self.owner.warm::<L::Views>();
            }
        }
    }

    let mut warm = Warm { owner, error: Ok(()) };
    R::visit_arms(&mut warm);
    warm.error
}

struct NameCheck {
    error: Option<Detail>,
}

impl ArmVisitor for NameCheck {
    fn visit<T, L, O>(&mut self, name: &'static str)
    where
        T: Trigger,
        L: Params<T>,
        O: Output,
    {
        if self.error.is_some() {
            return;
        }
        if let Err(error) = RuleName::new(name) {
            self.error = Some(Detail::new(format!("{error}")));
        }
    }
}
