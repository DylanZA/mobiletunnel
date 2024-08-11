/*
This file is part of MobileTunnel.
MobileTunnel is free software: you can redistribute it and/or modify it
under the terms of the GNU General Public License as published by the
Free Software Foundation, either version 3 of the License, or
(at your option) any later version.

MobileTunnel is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY; without even the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.

 See the GNU General Public License for more details.
You should have received a copy of the GNU General Public License
along with MobileTunnel. If not, see <https://www.gnu.org/licenses/>.

Copyright 2024 Dylan Yudaken
*/

use std::fmt::Display;

pub trait SwallowResultPrintErrExt {
    fn swallow_or_print_err<S>(self, prefix: S) -> ()
    where
        S: Display;
}

impl<E> SwallowResultPrintErrExt for Result<(), E>
where
    E: ::std::fmt::Debug,
{
    fn swallow_or_print_err<S>(self, prefix: S) -> ()
    where
        S: Display,
    {
        if let Err(e) = self {
            log::error!("{}: {:?}", prefix, e);
        }
        ()
    }
}
